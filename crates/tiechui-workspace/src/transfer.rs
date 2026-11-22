use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::{self, File};
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::STANDARD};
use flate2::Compression;
use flate2::write::DeflateEncoder;
use sha2::{Digest, Sha256};
use tempfile::NamedTempFile;
use tiechui_protocol::workspace::{
    DownloadReadParams, DownloadReadResult, DownloadStartResult, FileStat, UploadFinishResult,
    UploadStartParams, UploadStartResult, UploadWriteParams, UploadWriteResult,
};

use crate::worktree::{clear_readonly, is_link_or_reparse, remove_worktree_dir_safely};
use crate::{Error, Result, fail, lock};

const DEFAULT_DOWNLOAD_CHUNK_BYTES: usize = 256 * 1024;
const MAX_DOWNLOAD_CHUNK_BYTES: usize = 4 * 1024 * 1024;
const STREAM_BUF_BYTES: usize = 64 * 1024;
const UTF8_FILENAME_FLAG: u16 = 0x0800;

pub fn stat_many(root: &Path, paths: &[String]) -> Result<Vec<FileStat>> {
    let canonical_root = canonical_root_dir(root)?;
    let mut out = Vec::with_capacity(paths.len());
    for raw in paths {
        if raw.is_empty() || raw == "." {
            out.push(FileStat {
                path: raw.clone(),
                exists: true,
                dir: true,
                size: 0,
            });
            continue;
        }
        let parts = parse_relative_parts(raw)?;
        let (parent, leaf) = resolve_parent_no_symlinks(&canonical_root, &parts)?;
        let target = parent.join(leaf);
        match fs::symlink_metadata(&target) {
            Ok(meta) => out.push(FileStat {
                path: raw.clone(),
                exists: true,
                dir: meta.is_dir() && !is_link_or_reparse(&meta),
                size: if meta.is_file() && !is_link_or_reparse(&meta) {
                    meta.len()
                } else {
                    0
                },
            }),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => out.push(FileStat {
                path: raw.clone(),
                exists: false,
                dir: false,
                size: 0,
            }),
            Err(err) => return Err(err.into()),
        }
    }
    Ok(out)
}

struct ActiveUpload {
    client_id: u64,
    canonical_root: PathBuf,
    parts: Vec<OsString>,
    rel_path: String,
    target: PathBuf,
    overwrite: bool,
    expected_size: Option<u64>,
    bytes_written: u64,
    hasher: Sha256,
    writer: Option<BufWriter<NamedTempFile>>,
    created_dirs: Vec<PathBuf>,
    committed: bool,
}

impl Drop for ActiveUpload {
    fn drop(&mut self) {
        let _ = self.writer.take();
        if !self.committed {
            for dir in self.created_dirs.iter().rev() {
                let _ = fs::remove_dir(dir);
            }
        }
    }
}

struct ActiveDownload {
    client_id: u64,
    reader: BufReader<File>,
    // Declared after `reader` so `reader` closes before `NamedTempFile` unlinks on Windows.
    _temp_archive: Option<NamedTempFile>,
}

pub(crate) struct TransferManager {
    home: Option<PathBuf>,
    next_id: AtomicU64,
    uploads: Mutex<HashMap<String, ActiveUpload>>,
    downloads: Mutex<HashMap<String, ActiveDownload>>,
    inflight_zips: Mutex<HashMap<String, (u64, Arc<AtomicBool>)>>,
}

impl TransferManager {
    pub(crate) fn new(home: Option<PathBuf>) -> Self {
        Self {
            home,
            next_id: AtomicU64::new(1),
            uploads: Mutex::default(),
            downloads: Mutex::default(),
            inflight_zips: Mutex::default(),
        }
    }

    fn alloc_id(&self, kind: &str) -> String {
        let seq = self.next_id.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        format!("{kind}-{seq}-{nanos:x}")
    }

    pub(crate) fn upload_start(
        &self,
        client_id: u64,
        root: &Path,
        params: &UploadStartParams,
    ) -> Result<UploadStartResult> {
        let canonical_root = canonical_root_dir(root)?;
        let parts = parse_relative_parts(&params.path)?;
        let rel_path = normalize_rel_display(&parts)?;
        let (parent, leaf, created_dirs) = ensure_parent_dirs_no_symlinks(&canonical_root, &parts)?;
        let target = parent.join(&leaf);
        if !params.overwrite && fs::symlink_metadata(&target).is_ok() {
            for dir in created_dirs.iter().rev() {
                let _ = fs::remove_dir(dir);
            }
            return fail(format!("{rel_path} already exists"));
        }
        let temp_file = match tempfile::Builder::new()
            .prefix(".tiechui-upload-")
            .suffix(".part")
            .tempfile_in(&parent)
        {
            Ok(file) => file,
            Err(err) => {
                for dir in created_dirs.iter().rev() {
                    let _ = fs::remove_dir(dir);
                }
                return Err(err.into());
            }
        };
        let transfer_id = self.alloc_id("up");
        let active = ActiveUpload {
            client_id,
            canonical_root,
            parts,
            rel_path,
            target,
            overwrite: params.overwrite,
            expected_size: params.size,
            bytes_written: 0,
            hasher: Sha256::new(),
            writer: Some(BufWriter::with_capacity(STREAM_BUF_BYTES, temp_file)),
            created_dirs,
            committed: false,
        };
        lock(&self.uploads).insert(transfer_id.clone(), active);
        Ok(UploadStartResult { transfer_id })
    }

    pub(crate) fn upload_write(
        &self,
        client_id: u64,
        params: &UploadWriteParams,
    ) -> Result<UploadWriteResult> {
        let chunk = STANDARD
            .decode(&params.data)
            .map_err(|err| Error(format!("invalid base64 upload chunk: {err}")))?;
        let mut uploads = lock(&self.uploads);
        let Some(entry) = uploads.get_mut(&params.transfer_id) else {
            return fail(format!("unknown upload transfer {}", params.transfer_id));
        };
        if client_id != 0 && entry.client_id != client_id {
            return fail("transfer belongs to another client");
        }
        let next_bytes = entry
            .bytes_written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| Error("upload size overflow".into()))?;
        if let Some(expected) = entry.expected_size
            && next_bytes > expected
        {
            uploads.remove(&params.transfer_id);
            return fail(format!(
                "upload exceeds declared size ({next_bytes} > {expected})"
            ));
        }
        let Some(writer) = entry.writer.as_mut() else {
            uploads.remove(&params.transfer_id);
            return fail("upload stream is closed");
        };
        if let Err(err) = writer.write_all(&chunk) {
            uploads.remove(&params.transfer_id);
            return Err(err.into());
        }
        entry.hasher.update(&chunk);
        entry.bytes_written = next_bytes;
        Ok(UploadWriteResult {
            bytes_written: next_bytes,
        })
    }

    pub(crate) fn upload_finish(
        &self,
        client_id: u64,
        transfer_id: &str,
    ) -> Result<UploadFinishResult> {
        let mut active = {
            let mut uploads = lock(&self.uploads);
            let Some(entry) = uploads.get(transfer_id) else {
                return fail(format!("unknown upload transfer {transfer_id}"));
            };
            if client_id != 0 && entry.client_id != client_id {
                return fail("transfer belongs to another client");
            }
            uploads
                .remove(transfer_id)
                .ok_or_else(|| Error(format!("unknown upload transfer {transfer_id}")))?
        };
        if let Some(expected) = active.expected_size
            && active.bytes_written != expected
        {
            return fail(format!(
                "upload size mismatch: expected {expected} bytes, received {}",
                active.bytes_written
            ));
        }
        verify_existing_parents_no_symlinks(&active.canonical_root, &active.parts)?;
        let mut writer = active
            .writer
            .take()
            .ok_or_else(|| Error("upload stream is closed".into()))?;
        writer.flush()?;
        let temp_file = writer.into_inner().map_err(|err| err.into_error())?;
        temp_file.as_file().sync_data()?;

        if active.overwrite {
            if let Ok(meta) = fs::symlink_metadata(&active.target) {
                if meta.is_dir() && !is_link_or_reparse(&meta) {
                    remove_worktree_dir_safely(&active.target)?;
                } else {
                    clear_readonly(&active.target, &meta);
                }
            }
            temp_file
                .persist(&active.target)
                .map_err(|err| Error(err.error.to_string()))?;
        } else {
            temp_file
                .persist_noclobber(&active.target)
                .map_err(|err| Error(format!("{}: {}", active.rel_path, err.error)))?;
        }
        active.committed = true;
        let sha256 = format!("{:x}", active.hasher.clone().finalize());
        Ok(UploadFinishResult {
            path: active.rel_path.clone(),
            size: active.bytes_written,
            sha256,
        })
    }

    pub(crate) fn upload_abort(&self, client_id: u64, transfer_id: &str) -> Result<()> {
        let mut uploads = lock(&self.uploads);
        if let Some(entry) = uploads.get(transfer_id) {
            if client_id != 0 && entry.client_id != client_id {
                return fail("transfer belongs to another client");
            }
            uploads.remove(transfer_id);
        }
        Ok(())
    }

    pub(crate) fn download_start(
        &self,
        client_id: u64,
        root: &Path,
        path: &str,
    ) -> Result<DownloadStartResult> {
        let canonical_root = canonical_root_dir(root)?;
        let target = if path.is_empty() || path == "." {
            canonical_root.clone()
        } else {
            let parts = parse_relative_parts(path)?;
            resolve_existing_no_symlinks(&canonical_root, &parts)?
        };
        let meta = fs::symlink_metadata(&target)?;
        if is_link_or_reparse(&meta) {
            return fail("symlink download is not allowed");
        }
        let transfer_id = self.alloc_id("dl");
        if meta.is_file() {
            let name = target
                .file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| Error("invalid file name".into()))?
                .to_owned();
            let file = File::open(&target)?;
            let total_bytes = file.metadata()?.len();
            lock(&self.downloads).insert(
                transfer_id.clone(),
                ActiveDownload {
                    client_id,
                    reader: BufReader::with_capacity(STREAM_BUF_BYTES, file),
                    _temp_archive: None,
                },
            );
            return Ok(DownloadStartResult {
                transfer_id,
                name,
                archive: false,
                total_bytes,
            });
        }
        if !meta.is_dir() {
            return fail(format!("{} is not a regular file or directory", path));
        }

        let dir_label = target
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("workspace");
        let archive_name = format!("{dir_label}.zip");
        let cancelled = Arc::new(AtomicBool::new(false));
        lock(&self.inflight_zips).insert(transfer_id.clone(), (client_id, cancelled.clone()));
        let built = build_directory_zip(self.home.as_deref(), &canonical_root, &target, &cancelled);
        lock(&self.inflight_zips).remove(&transfer_id);
        let (temp_archive, total_bytes) = built?;
        if cancelled.load(Ordering::Relaxed) {
            return fail("download cancelled");
        }
        let mut reader_file = temp_archive.as_file().try_clone()?;
        reader_file.seek(SeekFrom::Start(0))?;
        lock(&self.downloads).insert(
            transfer_id.clone(),
            ActiveDownload {
                client_id,
                reader: BufReader::with_capacity(STREAM_BUF_BYTES, reader_file),
                _temp_archive: Some(temp_archive),
            },
        );
        Ok(DownloadStartResult {
            transfer_id,
            name: archive_name,
            archive: true,
            total_bytes,
        })
    }

    pub(crate) fn download_read(
        &self,
        client_id: u64,
        params: &DownloadReadParams,
    ) -> Result<DownloadReadResult> {
        let chunk_cap = params
            .chunk_size
            .map(|n| (n as usize).clamp(1, MAX_DOWNLOAD_CHUNK_BYTES))
            .unwrap_or(DEFAULT_DOWNLOAD_CHUNK_BYTES);
        let mut downloads = lock(&self.downloads);
        let Some(entry) = downloads.get_mut(&params.transfer_id) else {
            return fail(format!("unknown download transfer {}", params.transfer_id));
        };
        if client_id != 0 && entry.client_id != client_id {
            return fail("transfer belongs to another client");
        }
        let mut buf = vec![0_u8; chunk_cap];
        let mut filled = 0_usize;
        while filled < chunk_cap {
            match entry.reader.read(&mut buf[filled..]) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(err) => {
                    downloads.remove(&params.transfer_id);
                    return Err(err.into());
                }
            }
        }
        buf.truncate(filled);
        let eof = if filled < chunk_cap {
            true
        } else {
            entry
                .reader
                .fill_buf()
                .map(|b| b.is_empty())
                .unwrap_or(false)
        };
        if eof {
            downloads.remove(&params.transfer_id);
        }
        Ok(DownloadReadResult {
            data: STANDARD.encode(&buf),
            bytes_read: filled as u32,
            eof,
        })
    }

    pub(crate) fn download_abort(&self, client_id: u64, transfer_id: &str) -> Result<()> {
        if let Some((owner, flag)) = lock(&self.inflight_zips).get(transfer_id)
            && (client_id == 0 || *owner == client_id)
        {
            flag.store(true, Ordering::Relaxed);
        }
        let mut downloads = lock(&self.downloads);
        if let Some(entry) = downloads.get(transfer_id) {
            if client_id != 0 && entry.client_id != client_id {
                return fail("transfer belongs to another client");
            }
            downloads.remove(transfer_id);
        }
        Ok(())
    }

    pub(crate) fn remove_client(&self, client_id: u64) {
        if client_id == 0 {
            return;
        }
        for (owner, flag) in lock(&self.inflight_zips).values() {
            if *owner == client_id {
                flag.store(true, Ordering::Relaxed);
            }
        }
        lock(&self.uploads).retain(|_, v| v.client_id != client_id);
        lock(&self.downloads).retain(|_, v| v.client_id != client_id);
    }

    pub(crate) fn close(&self) {
        for (_, (_, flag)) in lock(&self.inflight_zips).drain() {
            flag.store(true, Ordering::Relaxed);
        }
        lock(&self.uploads).clear();
        lock(&self.downloads).clear();
    }
}

fn canonical_root_dir(root: &Path) -> Result<PathBuf> {
    if !root.is_absolute() {
        return fail(format!("{} is not an absolute path", root.display()));
    }
    let canon = root.canonicalize()?;
    if !canon.is_dir() {
        return fail(format!("{} is not a directory", root.display()));
    }
    Ok(canon)
}

fn parse_relative_parts(raw: &str) -> Result<Vec<OsString>> {
    if raw.is_empty() || raw.starts_with('/') || raw.starts_with('\\') {
        return fail(format!("{raw} is not a relative path inside the project"));
    }
    let normalized = raw.replace('\\', "/");
    let mut parts = Vec::new();
    for comp in Path::new(&normalized).components() {
        match comp {
            Component::Normal(part) if part != "." && part != ".." && !part.is_empty() => {
                parts.push(part.to_owned());
            }
            _ => return fail(format!("{raw} is not a relative path inside the project")),
        }
    }
    if parts.is_empty() {
        return fail(format!("{raw} is not a relative path inside the project"));
    }
    Ok(parts)
}

fn normalize_rel_display(parts: &[OsString]) -> Result<String> {
    let mut strs = Vec::with_capacity(parts.len());
    for part in parts {
        let s = part
            .to_str()
            .ok_or_else(|| Error("non-UTF-8 relative path".into()))?;
        strs.push(s);
    }
    Ok(strs.join("/"))
}

fn resolve_parent_no_symlinks(
    canonical_root: &Path,
    parts: &[OsString],
) -> Result<(PathBuf, OsString)> {
    let (leaf, parents) = parts
        .split_last()
        .ok_or_else(|| Error("empty relative path".into()))?;
    let mut cur = canonical_root.to_path_buf();
    for part in parents {
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) => {
                if is_link_or_reparse(&meta) {
                    return fail("symlink traversal is not allowed");
                }
                if !meta.is_dir() {
                    return fail(format!("{} is not a directory", cur.display()));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.into()),
        }
    }
    Ok((cur, leaf.clone()))
}

fn ensure_parent_dirs_no_symlinks(
    canonical_root: &Path,
    parts: &[OsString],
) -> Result<(PathBuf, OsString, Vec<PathBuf>)> {
    let (leaf, parents) = parts
        .split_last()
        .ok_or_else(|| Error("empty relative path".into()))?;
    let mut cur = canonical_root.to_path_buf();
    let mut created = Vec::new();
    for part in parents {
        cur.push(part);
        match fs::symlink_metadata(&cur) {
            Ok(meta) => {
                if is_link_or_reparse(&meta) {
                    for dir in created.iter().rev() {
                        let _ = fs::remove_dir(dir);
                    }
                    return fail("symlink traversal is not allowed");
                }
                if !meta.is_dir() {
                    for dir in created.iter().rev() {
                        let _ = fs::remove_dir(dir);
                    }
                    return fail(format!("{} is not a directory", cur.display()));
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                if let Err(create_err) = fs::create_dir(&cur) {
                    for dir in created.iter().rev() {
                        let _ = fs::remove_dir(dir);
                    }
                    return Err(create_err.into());
                }
                created.push(cur.clone());
            }
            Err(err) => {
                for dir in created.iter().rev() {
                    let _ = fs::remove_dir(dir);
                }
                return Err(err.into());
            }
        }
    }
    Ok((cur, leaf.clone(), created))
}

fn verify_existing_parents_no_symlinks(canonical_root: &Path, parts: &[OsString]) -> Result<()> {
    let (_, parents) = parts
        .split_last()
        .ok_or_else(|| Error("empty relative path".into()))?;
    let mut cur = canonical_root.to_path_buf();
    for part in parents {
        cur.push(part);
        let meta = fs::symlink_metadata(&cur)?;
        if is_link_or_reparse(&meta) || !meta.is_dir() {
            return fail("upload parent directory changed unexpectedly");
        }
    }
    Ok(())
}

fn resolve_existing_no_symlinks(canonical_root: &Path, parts: &[OsString]) -> Result<PathBuf> {
    let mut cur = canonical_root.to_path_buf();
    for (idx, part) in parts.iter().enumerate() {
        cur.push(part);
        let meta = fs::symlink_metadata(&cur)?;
        if is_link_or_reparse(&meta) {
            return fail("symlink traversal is not allowed");
        }
        if idx + 1 < parts.len() && !meta.is_dir() {
            return fail(format!("{} is not a directory", cur.display()));
        }
    }
    Ok(cur)
}

struct ZipEntryMeta {
    name: String,
    method: u16,
    dos_time: u16,
    dos_date: u16,
    crc32: u32,
    compressed_size: u32,
    uncompressed_size: u32,
    local_header_offset: u32,
    external_attrs: u32,
}

fn build_directory_zip(
    home: Option<&Path>,
    canonical_root: &Path,
    target_dir: &Path,
    cancelled: &AtomicBool,
) -> Result<(NamedTempFile, u64)> {
    let excluded_wt = crate::fs::excluded_managed_worktrees(home, canonical_root);
    let mut temp = create_zip_tempfile(home)?;
    let mut entries = Vec::new();
    collect_and_write_zip_dir(
        canonical_root,
        target_dir,
        target_dir,
        &excluded_wt,
        cancelled,
        temp.as_file_mut(),
        &mut entries,
    )?;
    write_central_directory(temp.as_file_mut(), &entries)?;
    temp.as_file_mut().flush()?;
    let total_bytes = temp.as_file().metadata()?.len();
    Ok((temp, total_bytes))
}

fn create_zip_tempfile(home: Option<&Path>) -> Result<NamedTempFile> {
    if let Ok(file) = tempfile::Builder::new()
        .prefix(".tiechui-zip-")
        .suffix(".zip")
        .tempfile()
    {
        return Ok(file);
    }
    if let Some(home) = home {
        let fallback_dir = home.join("tmp");
        tiechui_config::private::create_dir_all(&fallback_dir)?;
        return Ok(tempfile::Builder::new()
            .prefix(".tiechui-zip-")
            .suffix(".zip")
            .tempfile_in(&fallback_dir)?);
    }
    fail("failed to create temporary archive file")
}

fn collect_and_write_zip_dir(
    canonical_root: &Path,
    archive_base: &Path,
    current_dir: &Path,
    excluded_wt: &[PathBuf],
    cancelled: &AtomicBool,
    out: &mut File,
    entries: &mut Vec<ZipEntryMeta>,
) -> Result<bool> {
    if cancelled.load(Ordering::Relaxed) {
        return fail("download cancelled");
    }
    let mut children = Vec::new();
    for item in fs::read_dir(current_dir)? {
        let item = item?;
        let path = item.path();
        if crate::fs::should_skip_workspace_entry(canonical_root, &path, excluded_wt) {
            continue;
        }
        let meta = match fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => return Err(err.into()),
        };
        if is_link_or_reparse(&meta) {
            continue;
        }
        children.push((path, meta));
    }
    children.sort_by(|(a, _), (b, _)| a.file_name().cmp(&b.file_name()));

    let mut emitted_any = false;
    for (path, meta) in children {
        if cancelled.load(Ordering::Relaxed) {
            return fail("download cancelled");
        }
        if meta.is_dir() {
            let child_emitted = collect_and_write_zip_dir(
                canonical_root,
                archive_base,
                &path,
                excluded_wt,
                cancelled,
                out,
                entries,
            )?;
            if child_emitted {
                emitted_any = true;
            } else if !is_worktrees_only_dot_tiechui(canonical_root, &path) {
                let rel = zip_rel_name(archive_base, &path, true)?;
                write_zip_dir_entry(out, &rel, &meta, entries)?;
                emitted_any = true;
            }
        } else if meta.is_file() {
            let rel = zip_rel_name(archive_base, &path, false)?;
            write_zip_file_entry(out, &path, &rel, &meta, cancelled, entries)?;
            emitted_any = true;
        }
    }
    Ok(emitted_any)
}

fn is_worktrees_only_dot_tiechui(canonical_root: &Path, dir: &Path) -> bool {
    dir == canonical_root.join(".tiechui") && dir.join("worktrees").exists()
}

fn zip_rel_name(base: &Path, path: &Path, is_dir: bool) -> Result<String> {
    let rel = path
        .strip_prefix(base)
        .map_err(|_| Error("archive entry outside base directory".into()))?;
    let mut parts = Vec::new();
    for comp in rel.components() {
        if let Component::Normal(s) = comp {
            let text = s
                .to_str()
                .ok_or_else(|| Error(format!("non-UTF-8 path in archive: {}", path.display())))?;
            parts.push(text);
        }
    }
    if parts.is_empty() {
        return fail("empty archive entry name");
    }
    let mut joined = parts.join("/");
    if is_dir {
        joined.push('/');
    }
    if joined.len() > usize::from(u16::MAX) {
        return fail("archive entry path exceeds ZIP limit");
    }
    Ok(joined)
}

fn write_zip_dir_entry(
    out: &mut File,
    name: &str,
    meta: &fs::Metadata,
    entries: &mut Vec<ZipEntryMeta>,
) -> Result<()> {
    let local_offset = u32::try_from(out.stream_position()?)
        .map_err(|_| Error("archive exceeds 4 GiB ZIP limit".into()))?;
    let (dos_time, dos_date) = dos_datetime(meta);
    write_local_header(out, name, 0, dos_time, dos_date)?;
    entries.push(ZipEntryMeta {
        name: name.to_owned(),
        method: 0,
        dos_time,
        dos_date,
        crc32: 0,
        compressed_size: 0,
        uncompressed_size: 0,
        local_header_offset: local_offset,
        external_attrs: (0o040755_u32 << 16) | 0x10,
    });
    Ok(())
}

fn write_zip_file_entry(
    out: &mut File,
    path: &Path,
    name: &str,
    meta: &fs::Metadata,
    cancelled: &AtomicBool,
    entries: &mut Vec<ZipEntryMeta>,
) -> Result<()> {
    let local_offset = u32::try_from(out.stream_position()?)
        .map_err(|_| Error("archive exceeds 4 GiB ZIP limit".into()))?;
    let (dos_time, dos_date) = dos_datetime(meta);
    let method: u16 = if meta.len() == 0 { 0 } else { 8 };
    write_local_header(out, name, method, dos_time, dos_date)?;

    let data_start = out.stream_position()?;
    let mut src = File::open(path)?;
    let mut crc_hasher = crc32fast::Hasher::new();
    let mut uncompressed: u64 = 0;
    let mut buf = [0_u8; STREAM_BUF_BYTES];

    if method == 8 {
        let mut encoder = DeflateEncoder::new(&mut *out, Compression::fast());
        loop {
            if cancelled.load(Ordering::Relaxed) {
                return fail("download cancelled");
            }
            let n = src.read(&mut buf)?;
            if n == 0 {
                break;
            }
            crc_hasher.update(&buf[..n]);
            encoder.write_all(&buf[..n])?;
            uncompressed = uncompressed
                .checked_add(n as u64)
                .ok_or_else(|| Error("file size overflow".into()))?;
        }
        encoder.finish()?;
    } else {
        let n = src.read(&mut buf)?;
        if n > 0 {
            crc_hasher.update(&buf[..n]);
            out.write_all(&buf[..n])?;
            uncompressed = n as u64;
        }
    }

    let data_end = out.stream_position()?;
    let compressed_u64 = data_end.saturating_sub(data_start);
    let crc32 = crc_hasher.finalize();
    let compressed_size = u32::try_from(compressed_u64)
        .map_err(|_| Error(format!("{name} exceeds 4 GiB ZIP entry limit")))?;
    let uncompressed_size = u32::try_from(uncompressed)
        .map_err(|_| Error(format!("{name} exceeds 4 GiB ZIP entry limit")))?;

    out.seek(SeekFrom::Start(u64::from(local_offset) + 14))?;
    out.write_all(&crc32.to_le_bytes())?;
    out.write_all(&compressed_size.to_le_bytes())?;
    out.write_all(&uncompressed_size.to_le_bytes())?;
    out.seek(SeekFrom::Start(data_end))?;

    entries.push(ZipEntryMeta {
        name: name.to_owned(),
        method,
        dos_time,
        dos_date,
        crc32,
        compressed_size,
        uncompressed_size,
        local_header_offset: local_offset,
        external_attrs: file_external_attrs(meta),
    });
    Ok(())
}

fn write_local_header(
    out: &mut File,
    name: &str,
    method: u16,
    dos_time: u16,
    dos_date: u16,
) -> Result<()> {
    let name_bytes = name.as_bytes();
    let name_len = name_bytes.len() as u16;
    out.write_all(&0x04034b50_u32.to_le_bytes())?;
    out.write_all(&20_u16.to_le_bytes())?;
    out.write_all(&UTF8_FILENAME_FLAG.to_le_bytes())?;
    out.write_all(&method.to_le_bytes())?;
    out.write_all(&dos_time.to_le_bytes())?;
    out.write_all(&dos_date.to_le_bytes())?;
    out.write_all(&[0_u8; 12])?;
    out.write_all(&name_len.to_le_bytes())?;
    out.write_all(&0_u16.to_le_bytes())?;
    out.write_all(name_bytes)?;
    Ok(())
}

fn write_central_directory(out: &mut File, entries: &[ZipEntryMeta]) -> Result<()> {
    let count = u16::try_from(entries.len())
        .map_err(|_| Error("directory exceeds 65535 ZIP entries limit".into()))?;
    let cd_offset_u64 = out.stream_position()?;
    let cd_offset = u32::try_from(cd_offset_u64)
        .map_err(|_| Error("archive exceeds 4 GiB ZIP limit".into()))?;

    for item in entries {
        let name_bytes = item.name.as_bytes();
        let name_len = name_bytes.len() as u16;
        out.write_all(&0x02014b50_u32.to_le_bytes())?;
        out.write_all(&((3_u16 << 8) | 20).to_le_bytes())?;
        out.write_all(&20_u16.to_le_bytes())?;
        out.write_all(&UTF8_FILENAME_FLAG.to_le_bytes())?;
        out.write_all(&item.method.to_le_bytes())?;
        out.write_all(&item.dos_time.to_le_bytes())?;
        out.write_all(&item.dos_date.to_le_bytes())?;
        out.write_all(&item.crc32.to_le_bytes())?;
        out.write_all(&item.compressed_size.to_le_bytes())?;
        out.write_all(&item.uncompressed_size.to_le_bytes())?;
        out.write_all(&name_len.to_le_bytes())?;
        out.write_all(&0_u16.to_le_bytes())?;
        out.write_all(&0_u16.to_le_bytes())?;
        out.write_all(&0_u16.to_le_bytes())?;
        out.write_all(&0_u16.to_le_bytes())?;
        out.write_all(&item.external_attrs.to_le_bytes())?;
        out.write_all(&item.local_header_offset.to_le_bytes())?;
        out.write_all(name_bytes)?;
    }

    let cd_end = out.stream_position()?;
    let cd_size = u32::try_from(cd_end.saturating_sub(cd_offset_u64))
        .map_err(|_| Error("central directory exceeds 4 GiB ZIP limit".into()))?;

    out.write_all(&0x06054b50_u32.to_le_bytes())?;
    out.write_all(&0_u16.to_le_bytes())?;
    out.write_all(&0_u16.to_le_bytes())?;
    out.write_all(&count.to_le_bytes())?;
    out.write_all(&count.to_le_bytes())?;
    out.write_all(&cd_size.to_le_bytes())?;
    out.write_all(&cd_offset.to_le_bytes())?;
    out.write_all(&0_u16.to_le_bytes())?;
    Ok(())
}

fn file_external_attrs(meta: &fs::Metadata) -> u32 {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if meta.permissions().mode() & 0o111 != 0 {
            return 0o100755_u32 << 16;
        }
    }
    #[cfg(not(unix))]
    let _ = meta;
    0o100644_u32 << 16
}

fn dos_datetime(meta: &fs::Metadata) -> (u16, u16) {
    let Ok(modified) = meta.modified() else {
        return (0, 0x0021);
    };
    let Ok(dur) = modified.duration_since(UNIX_EPOCH) else {
        return (0, 0x0021);
    };
    let secs = dur.as_secs();
    let millis = secs.saturating_mul(1000);
    let (year, month, day) = crate::worktree::utc_ymd(millis);
    if !(1980..=2107).contains(&year) {
        return (0, 0x0021);
    }
    let rem = secs % 86_400;
    let hour = (rem / 3600) as u16;
    let min = ((rem % 3600) / 60) as u16;
    let sec2 = ((rem % 60) / 2) as u16;
    let dos_time = (hour << 11) | (min << 5) | sec2;
    let dos_date = (((year - 1980) as u16) << 9) | ((month as u16) << 5) | (day as u16);
    (dos_time, dos_date)
}
