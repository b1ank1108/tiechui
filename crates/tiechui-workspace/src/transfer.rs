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
    paths
        .iter()
        .map(|raw| stat_one(&canonical_root, raw))
        .collect()
}

fn stat_one(canonical_root: &Path, raw: &str) -> Result<FileStat> {
    let mut stat = FileStat {
        path: raw.to_owned(),
        exists: true,
        dir: true,
        size: 0,
    };
    if raw.is_empty() || raw == "." {
        return Ok(stat);
    }
    let (parent, leaf) = resolve_parent_no_symlinks(canonical_root, &parse_relative_parts(raw)?)?;
    let meta = metadata_if_exists(&parent.join(leaf))?;
    let real = meta.as_ref().filter(|meta| !is_link_or_reparse(meta));
    stat.exists = meta.is_some();
    stat.dir = real.is_some_and(|meta| meta.is_dir());
    stat.size = real
        .filter(|meta| meta.is_file())
        .map_or(0, |meta| meta.len());
    Ok(stat)
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
            remove_dirs(&self.created_dirs);
        }
    }
}

impl ActiveUpload {
    fn append(&mut self, chunk: &[u8]) -> Result<u64> {
        let next = self
            .bytes_written
            .checked_add(chunk.len() as u64)
            .ok_or_else(|| Error("upload size overflow".into()))?;
        if let Some(expected) = self.expected_size
            && next > expected
        {
            return fail(format!(
                "upload exceeds declared size ({next} > {expected})"
            ));
        }
        let writer = self
            .writer
            .as_mut()
            .ok_or_else(|| Error("upload stream is closed".into()))?;
        writer.write_all(chunk)?;
        self.hasher.update(chunk);
        self.bytes_written = next;
        Ok(next)
    }

    /// Checks the declared size and parent chain, then syncs the temp file to disk.
    fn seal(&mut self) -> Result<NamedTempFile> {
        if let Some(expected) = self.expected_size
            && self.bytes_written != expected
        {
            return fail(format!(
                "upload size mismatch: expected {expected} bytes, received {}",
                self.bytes_written
            ));
        }
        verify_existing_parents_no_symlinks(&self.canonical_root, &self.parts)?;
        let writer = self
            .writer
            .take()
            .ok_or_else(|| Error("upload stream is closed".into()))?;
        let temp_file = writer.into_inner().map_err(|err| err.into_error())?;
        temp_file.as_file().sync_data()?;
        Ok(temp_file)
    }

    fn persist(&mut self, temp_file: NamedTempFile) -> Result<()> {
        if self.overwrite {
            clear_overwrite_target(&self.target)?;
            temp_file
                .persist(&self.target)
                .map_err(|err| Error(err.error.to_string()))?;
        } else {
            temp_file
                .persist_noclobber(&self.target)
                .map_err(|err| Error(format!("{}: {}", self.rel_path, err.error)))?;
        }
        self.committed = true;
        Ok(())
    }
}

struct ActiveDownload {
    client_id: u64,
    reader: BufReader<File>,
    // Declared after `reader` so `reader` closes before `NamedTempFile` unlinks on Windows.
    _temp_archive: Option<NamedTempFile>,
}

/// Download name, the stream to read from, and the temp archive backing it, if any.
type DownloadSource = (String, File, Option<NamedTempFile>);

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
        // Dropping `active` on an early return removes the directories created above.
        let mut active = ActiveUpload {
            client_id,
            canonical_root,
            parts,
            rel_path,
            target: parent.join(&leaf),
            overwrite: params.overwrite,
            expected_size: params.size,
            bytes_written: 0,
            hasher: Sha256::new(),
            writer: None,
            created_dirs,
            committed: false,
        };
        if !params.overwrite && fs::symlink_metadata(&active.target).is_ok() {
            return fail(format!("{} already exists", active.rel_path));
        }
        let temp_file = tempfile::Builder::new()
            .prefix(".tiechui-upload-")
            .suffix(".part")
            .tempfile_in(&parent)?;
        active.writer = Some(BufWriter::with_capacity(STREAM_BUF_BYTES, temp_file));
        let transfer_id = self.alloc_id("up");
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
        check_owner(entry.client_id, client_id)?;
        let written = entry.append(&chunk);
        if written.is_err() {
            uploads.remove(&params.transfer_id);
        }
        Ok(UploadWriteResult {
            bytes_written: written?,
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
            check_owner(entry.client_id, client_id)?;
            uploads
                .remove(transfer_id)
                .ok_or_else(|| Error(format!("unknown upload transfer {transfer_id}")))?
        };
        let temp_file = active.seal()?;
        active.persist(temp_file)?;
        Ok(UploadFinishResult {
            path: active.rel_path.clone(),
            size: active.bytes_written,
            sha256: format!("{:x}", active.hasher.clone().finalize()),
        })
    }

    pub(crate) fn upload_abort(&self, client_id: u64, transfer_id: &str) -> Result<()> {
        let mut uploads = lock(&self.uploads);
        if let Some(entry) = uploads.get(transfer_id) {
            check_owner(entry.client_id, client_id)?;
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
        let (target, meta) = resolve_download_target(&canonical_root, path)?;
        let transfer_id = self.alloc_id("dl");
        let (name, file, temp_archive) = if meta.is_file() {
            open_file_download(&target)?
        } else if meta.is_dir() {
            self.zip_for_download(client_id, &transfer_id, &canonical_root, &target)?
        } else {
            return fail(format!("{path} is not a regular file or directory"));
        };
        let total_bytes = file.metadata()?.len();
        let archive = temp_archive.is_some();
        lock(&self.downloads).insert(
            transfer_id.clone(),
            ActiveDownload {
                client_id,
                reader: BufReader::with_capacity(STREAM_BUF_BYTES, file),
                _temp_archive: temp_archive,
            },
        );
        Ok(DownloadStartResult {
            transfer_id,
            name,
            archive,
            total_bytes,
        })
    }

    /// Builds the archive while `download_abort` can still cancel it through `inflight_zips`.
    fn zip_for_download(
        &self,
        client_id: u64,
        transfer_id: &str,
        canonical_root: &Path,
        target: &Path,
    ) -> Result<DownloadSource> {
        let cancelled = Arc::new(AtomicBool::new(false));
        lock(&self.inflight_zips).insert(transfer_id.to_owned(), (client_id, cancelled.clone()));
        let built = build_directory_zip(self.home.as_deref(), canonical_root, target, &cancelled);
        lock(&self.inflight_zips).remove(transfer_id);
        let temp_archive = built?;
        ensure_not_cancelled(&cancelled)?;
        let mut file = temp_archive.as_file().try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let label = target
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("workspace");
        Ok((format!("{label}.zip"), file, Some(temp_archive)))
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
        check_owner(entry.client_id, client_id)?;
        let read = read_chunk(&mut entry.reader, chunk_cap);
        if !matches!(read, Ok((_, false))) {
            downloads.remove(&params.transfer_id);
        }
        let (data, eof) = read?;
        Ok(DownloadReadResult {
            bytes_read: data.len() as u32,
            data: STANDARD.encode(&data),
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
            check_owner(entry.client_id, client_id)?;
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

fn check_owner(owner: u64, client_id: u64) -> Result<()> {
    if client_id != 0 && owner != client_id {
        return fail("transfer belongs to another client");
    }
    Ok(())
}

fn ensure_not_cancelled(cancelled: &AtomicBool) -> Result<()> {
    if cancelled.load(Ordering::Relaxed) {
        return fail("download cancelled");
    }
    Ok(())
}

/// Reads up to `cap` bytes; the flag is set once the reader has nothing left.
fn read_chunk(reader: &mut BufReader<File>, cap: usize) -> std::io::Result<(Vec<u8>, bool)> {
    let mut buf = Vec::with_capacity(cap);
    reader.by_ref().take(cap as u64).read_to_end(&mut buf)?;
    let eof = buf.len() < cap || reader.fill_buf().map(|b| b.is_empty()).unwrap_or(false);
    Ok((buf, eof))
}

fn resolve_download_target(canonical_root: &Path, path: &str) -> Result<(PathBuf, fs::Metadata)> {
    let target = if path.is_empty() || path == "." {
        canonical_root.to_path_buf()
    } else {
        resolve_existing_no_symlinks(canonical_root, &parse_relative_parts(path)?)?
    };
    let meta = fs::symlink_metadata(&target)?;
    if is_link_or_reparse(&meta) {
        return fail("symlink download is not allowed");
    }
    Ok((target, meta))
}

fn open_file_download(target: &Path) -> Result<DownloadSource> {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error("invalid file name".into()))?;
    Ok((name.to_owned(), File::open(target)?, None))
}

fn clear_overwrite_target(target: &Path) -> Result<()> {
    if let Ok(meta) = fs::symlink_metadata(target) {
        if meta.is_dir() && !is_link_or_reparse(&meta) {
            remove_worktree_dir_safely(target)?;
        } else {
            clear_readonly(target, &meta);
        }
    }
    Ok(())
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
    let normalized = raw.replace('\\', "/");
    let parts: Option<Vec<OsString>> = Path::new(&normalized)
        .components()
        .map(|comp| match comp {
            Component::Normal(part) => Some(part.to_owned()),
            _ => None,
        })
        .collect();
    match parts {
        Some(parts) if !parts.is_empty() => Ok(parts),
        _ => fail(format!("{raw} is not a relative path inside the project")),
    }
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
        existing_real_dir(&cur)?;
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
        if let Err(err) = create_missing_dir(&cur, &mut created) {
            remove_dirs(&created);
            return Err(err);
        }
    }
    Ok((cur, leaf.clone(), created))
}

fn create_missing_dir(dir: &Path, created: &mut Vec<PathBuf>) -> Result<()> {
    if !existing_real_dir(dir)? {
        fs::create_dir(dir)?;
        created.push(dir.to_path_buf());
    }
    Ok(())
}

fn remove_dirs(dirs: &[PathBuf]) {
    for dir in dirs.iter().rev() {
        let _ = fs::remove_dir(dir);
    }
}

/// Whether `dir` exists; an existing entry must be a real directory.
fn existing_real_dir(dir: &Path) -> Result<bool> {
    let Some(meta) = metadata_if_exists(dir)? else {
        return Ok(false);
    };
    if is_link_or_reparse(&meta) {
        return fail("symlink traversal is not allowed");
    }
    if !meta.is_dir() {
        return fail(format!("{} is not a directory", dir.display()));
    }
    Ok(true)
}

fn metadata_if_exists(path: &Path) -> Result<Option<fs::Metadata>> {
    match fs::symlink_metadata(path) {
        Ok(meta) => Ok(Some(meta)),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(err) => Err(err.into()),
    }
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
) -> Result<NamedTempFile> {
    let excluded_wt = crate::fs::excluded_managed_worktrees(home, canonical_root);
    let mut temp = create_zip_tempfile(home)?;
    let mut zip = ZipBuilder {
        canonical_root,
        base: target_dir,
        excluded_wt: &excluded_wt,
        cancelled,
        out: temp.as_file_mut(),
        entries: Vec::new(),
    };
    zip.add_dir(target_dir)?;
    write_central_directory(zip.out, &zip.entries)?;
    temp.as_file_mut().flush()?;
    Ok(temp)
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

struct ZipBuilder<'a> {
    canonical_root: &'a Path,
    base: &'a Path,
    excluded_wt: &'a [PathBuf],
    cancelled: &'a AtomicBool,
    out: &'a mut File,
    entries: Vec<ZipEntryMeta>,
}

impl ZipBuilder<'_> {
    /// Writes the entries under `dir`; returns whether any were written.
    fn add_dir(&mut self, dir: &Path) -> Result<bool> {
        ensure_not_cancelled(self.cancelled)?;
        let mut emitted = false;
        for (path, meta) in list_zip_children(self.canonical_root, dir, self.excluded_wt)? {
            ensure_not_cancelled(self.cancelled)?;
            emitted |= self.add_child(&path, &meta)?;
        }
        Ok(emitted)
    }

    fn add_child(&mut self, path: &Path, meta: &fs::Metadata) -> Result<bool> {
        if meta.is_file() {
            let name = zip_rel_name(self.base, path, false)?;
            self.add_file(path, &name, meta)?;
            return Ok(true);
        }
        Ok(meta.is_dir() && self.add_subdir(path, meta)?)
    }

    /// Empty directories keep an entry, except a `.tiechui` that only held managed worktrees.
    fn add_subdir(&mut self, path: &Path, meta: &fs::Metadata) -> Result<bool> {
        if self.add_dir(path)? {
            return Ok(true);
        }
        if crate::fs::is_worktrees_holder(self.canonical_root, path) {
            return Ok(false);
        }
        let name = zip_rel_name(self.base, path, true)?;
        let local_header_offset = archive_offset(self.out)?;
        let (dos_time, dos_date) = dos_datetime(meta);
        self.out
            .write_all(&local_header(&name, 0, dos_time, dos_date))?;
        self.entries.push(ZipEntryMeta {
            name,
            method: 0,
            dos_time,
            dos_date,
            crc32: 0,
            compressed_size: 0,
            uncompressed_size: 0,
            local_header_offset,
            external_attrs: (0o040755_u32 << 16) | 0x10,
        });
        Ok(true)
    }

    fn add_file(&mut self, path: &Path, name: &str, meta: &fs::Metadata) -> Result<()> {
        let local_header_offset = archive_offset(self.out)?;
        let (dos_time, dos_date) = dos_datetime(meta);
        let method: u16 = if meta.len() == 0 { 0 } else { 8 };
        self.out
            .write_all(&local_header(name, method, dos_time, dos_date))?;
        let (crc32, uncompressed, compressed) =
            write_entry_data(self.out, path, method == 8, self.cancelled)?;
        let too_large = || Error(format!("{name} exceeds 4 GiB ZIP entry limit"));
        let compressed_size = u32::try_from(compressed).map_err(|_| too_large())?;
        let uncompressed_size = u32::try_from(uncompressed).map_err(|_| too_large())?;
        backfill_local_header(
            self.out,
            local_header_offset,
            [crc32, compressed_size, uncompressed_size],
        )?;
        self.entries.push(ZipEntryMeta {
            name: name.to_owned(),
            method,
            dos_time,
            dos_date,
            crc32,
            compressed_size,
            uncompressed_size,
            local_header_offset,
            external_attrs: file_external_attrs(meta),
        });
        Ok(())
    }
}

fn list_zip_children(
    canonical_root: &Path,
    dir: &Path,
    excluded_wt: &[PathBuf],
) -> Result<Vec<(PathBuf, fs::Metadata)>> {
    let mut children = Vec::new();
    for item in fs::read_dir(dir)? {
        let path = item?.path();
        if crate::fs::should_skip_workspace_entry(canonical_root, &path, excluded_wt) {
            continue;
        }
        if let Some(meta) = metadata_if_exists(&path)?.filter(|meta| !is_link_or_reparse(meta)) {
            children.push((path, meta));
        }
    }
    children.sort_by(|(a, _), (b, _)| a.file_name().cmp(&b.file_name()));
    Ok(children)
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

fn archive_offset(out: &mut File) -> Result<u32> {
    u32::try_from(out.stream_position()?)
        .map_err(|_| Error("archive exceeds 4 GiB ZIP limit".into()))
}

/// Streams `path` into `out`, deflated or stored; returns its CRC-32, source size and written size.
fn write_entry_data(
    out: &mut File,
    path: &Path,
    deflate: bool,
    cancelled: &AtomicBool,
) -> Result<(u32, u64, u64)> {
    let start = out.stream_position()?;
    let mut src = File::open(path)?;
    let (crc32, size) = if deflate {
        let mut encoder = DeflateEncoder::new(&mut *out, Compression::fast());
        let sums = copy_with_crc(&mut src, &mut encoder, cancelled)?;
        encoder.finish()?;
        sums
    } else {
        copy_with_crc(&mut src, out, cancelled)?
    };
    Ok((crc32, size, out.stream_position()?.saturating_sub(start)))
}

fn copy_with_crc(
    src: &mut File,
    dst: &mut impl Write,
    cancelled: &AtomicBool,
) -> Result<(u32, u64)> {
    let mut crc = crc32fast::Hasher::new();
    let mut size: u64 = 0;
    let mut buf = [0_u8; STREAM_BUF_BYTES];
    loop {
        ensure_not_cancelled(cancelled)?;
        let n = src.read(&mut buf)?;
        if n == 0 {
            return Ok((crc.finalize(), size));
        }
        crc.update(&buf[..n]);
        dst.write_all(&buf[..n])?;
        size = size
            .checked_add(n as u64)
            .ok_or_else(|| Error("file size overflow".into()))?;
    }
}

/// Fills CRC-32, compressed and uncompressed sizes into the local header at `offset`.
fn backfill_local_header(out: &mut File, offset: u32, fields: [u32; 3]) -> Result<()> {
    let end = out.stream_position()?;
    out.seek(SeekFrom::Start(u64::from(offset) + 14))?;
    out.write_all(&fields.map(u32::to_le_bytes).concat())?;
    out.seek(SeekFrom::Start(end))?;
    Ok(())
}

fn local_header(name: &str, method: u16, dos_time: u16, dos_date: u16) -> Vec<u8> {
    let mut header = Vec::with_capacity(30 + name.len());
    let mut put = |bytes: &[u8]| header.extend_from_slice(bytes);
    put(&0x04034b50_u32.to_le_bytes());
    put(&20_u16.to_le_bytes());
    put(&UTF8_FILENAME_FLAG.to_le_bytes());
    put(&method.to_le_bytes());
    put(&dos_time.to_le_bytes());
    put(&dos_date.to_le_bytes());
    put(&[0_u8; 12]);
    put(&(name.len() as u16).to_le_bytes());
    put(&0_u16.to_le_bytes());
    put(name.as_bytes());
    header
}

fn central_record(item: &ZipEntryMeta) -> Vec<u8> {
    let mut record = Vec::with_capacity(46 + item.name.len());
    let mut put = |bytes: &[u8]| record.extend_from_slice(bytes);
    put(&0x02014b50_u32.to_le_bytes());
    put(&((3_u16 << 8) | 20).to_le_bytes());
    put(&20_u16.to_le_bytes());
    put(&UTF8_FILENAME_FLAG.to_le_bytes());
    put(&item.method.to_le_bytes());
    put(&item.dos_time.to_le_bytes());
    put(&item.dos_date.to_le_bytes());
    put(&item.crc32.to_le_bytes());
    put(&item.compressed_size.to_le_bytes());
    put(&item.uncompressed_size.to_le_bytes());
    put(&(item.name.len() as u16).to_le_bytes());
    put(&[0_u8; 8]);
    put(&item.external_attrs.to_le_bytes());
    put(&item.local_header_offset.to_le_bytes());
    put(item.name.as_bytes());
    record
}

fn write_central_directory(out: &mut File, entries: &[ZipEntryMeta]) -> Result<()> {
    let count = u16::try_from(entries.len())
        .map_err(|_| Error("directory exceeds 65535 ZIP entries limit".into()))?;
    let cd_offset = archive_offset(out)?;
    let central: Vec<u8> = entries.iter().flat_map(central_record).collect();
    let cd_size = u32::try_from(central.len())
        .map_err(|_| Error("central directory exceeds 4 GiB ZIP limit".into()))?;
    let mut end = Vec::with_capacity(22);
    let mut put = |bytes: &[u8]| end.extend_from_slice(bytes);
    put(&0x06054b50_u32.to_le_bytes());
    put(&[0_u8; 4]);
    put(&count.to_le_bytes());
    put(&count.to_le_bytes());
    put(&cd_size.to_le_bytes());
    put(&cd_offset.to_le_bytes());
    put(&0_u16.to_le_bytes());
    out.write_all(&central)?;
    out.write_all(&end)?;
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
