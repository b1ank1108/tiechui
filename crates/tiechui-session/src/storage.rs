use std::collections::HashMap;
use std::fs::{self, DirBuilder, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use tokio_util::sync::CancellationToken;

use crate::{Diagnostic, Entry, FORMAT_VERSION, Header, SessionError, SessionStore, check_cancel};

/// Percent-encode the canonical cwd, shortening long names with a stable hash.
pub fn session_dir(home: &Path, cwd: &Path) -> Result<PathBuf, SessionError> {
    let cwd = canonical_cwd(cwd)?;
    Ok(home
        .join("sessions")
        .join(encode_cwd(cwd.as_os_str().as_encoded_bytes())))
}

fn encode_cwd(bytes: &[u8]) -> String {
    let mut encoded = String::new();
    for &byte in bytes {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.') {
            encoded.push(char::from(byte));
        } else {
            use std::fmt::Write;
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    if encoded.len() > 120 {
        let mut end = 120;
        if let Some(escape) = encoded[..end].rfind('%')
            && end - escape < 3
        {
            end = escape;
        }
        encoded.truncate(end);
        // FNV-1a is fixed across Rust versions, unlike std's default hasher.
        let mut hash = 0xcbf29ce484222325_u64;
        for &byte in bytes {
            hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
        }
        encoded.push_str(&format!("-{hash:016x}"));
    }
    encoded
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionInfo {
    pub path: PathBuf,
    pub header: Header,
}

#[derive(Debug, Default)]
pub struct SessionList {
    pub sessions: Vec<SessionInfo>,
    /// Files that could not be inspected or whose headers could not be read or parsed.
    pub diagnostics: Vec<SessionError>,
}

/// Read only the first line of each JSONL file; missing directories are empty lists.
/// Per-file failures are diagnostics; directory failures and cancellation are errors.
pub fn list(
    home: &Path,
    cwd: &Path,
    cancel: &CancellationToken,
) -> Result<SessionList, SessionError> {
    list_in(&session_dir(home, cwd)?, cancel)
}

pub(crate) fn list_all(
    home: &Path,
    cancel: &CancellationToken,
) -> Result<SessionList, SessionError> {
    let root = home.join("sessions");
    check_cancel(&root, cancel)?;
    let directories = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionList::default());
        }
        Err(error) => return Err(SessionError::new(&root, error)),
    };
    let mut result = SessionList::default();
    for directory in directories {
        check_cancel(&root, cancel)?;
        let directory = directory.map_err(|error| SessionError::new(&root, error))?;
        if directory
            .file_type()
            .map_err(|error| SessionError::new(&root, error))?
            .is_dir()
        {
            let list = list_in(&directory.path(), cancel)?;
            result.sessions.extend(list.sessions);
            result.diagnostics.extend(list.diagnostics);
        }
    }
    result
        .sessions
        .sort_by(|left, right| left.path.cmp(&right.path));
    result
        .diagnostics
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(result)
}

/// Resolve dot segments without replacing the user's symlink spelling.
pub fn resolve_cwd(cwd: &Path) -> Result<PathBuf, SessionError> {
    let absolute = std::path::absolute(cwd).map_err(|error| SessionError::new(cwd, error))?;
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            std::path::Component::ParentDir => {
                resolved.pop();
            }
            std::path::Component::CurDir => {}
            component => resolved.push(component.as_os_str()),
        }
    }
    canonical_cwd(&resolved)?;
    Ok(resolved)
}

pub(crate) fn list_in(
    directory: &Path,
    cancel: &CancellationToken,
) -> Result<SessionList, SessionError> {
    check_cancel(directory, cancel)?;
    let files = match fs::read_dir(directory) {
        Ok(files) => files,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(SessionList::default());
        }
        Err(error) => return Err(SessionError::new(directory, error)),
    };
    let mut result = SessionList::default();
    for file in files {
        check_cancel(directory, cancel)?;
        let file = match file {
            Ok(file) => file,
            Err(error) => {
                result.diagnostics.push(SessionError::new(directory, error));
                continue;
            }
        };
        let path = file.path();
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        let header = (|| {
            if !file
                .file_type()
                .map_err(|error| SessionError::new(&path, error))?
                .is_file()
            {
                return Ok(None);
            }
            let mut reader =
                BufReader::new(File::open(&path).map_err(|error| SessionError::new(&path, error))?);
            let mut line = Vec::new();
            reader
                .read_until(b'\n', &mut line)
                .map_err(|error| SessionError::at_line(&path, 1, error))?;
            parse_header(&path, &line).map(Some)
        })();
        match header {
            Ok(Some(header)) => result.sessions.push(SessionInfo { header, path }),
            Ok(None) => {}
            Err(error) => result.diagnostics.push(error),
        }
    }
    result
        .sessions
        .sort_by(|left, right| left.path.cmp(&right.path));
    result
        .diagnostics
        .sort_by(|left, right| left.path.cmp(&right.path));
    Ok(result)
}

impl SessionStore {
    pub(crate) fn create_in(
        directory: &Path,
        cwd: &Path,
        parent_session: Option<PathBuf>,
        cancel: &CancellationToken,
    ) -> Result<Self, SessionError> {
        check_cancel(directory, cancel)?;
        let cwd = resolve_cwd(cwd)?;
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder
            .create(directory)
            .map_err(|error| SessionError::new(directory, error))?;
        let directory = directory
            .canonicalize()
            .map_err(|error| SessionError::new(directory, error))?;
        let id = new_id();
        let path = directory.join(format!("{id}.jsonl"));
        let mut options = OpenOptions::new();
        options.read(true).append(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&path)
            .map_err(|error| SessionError::new(&path, error))?;
        let header = Header {
            format_version: FORMAT_VERSION,
            id,
            cwd,
            created_at: now_ms(),
            parent_session,
        };
        let result = (|| {
            file.try_lock().map_err(|error| {
                SessionError::new(&path, format!("cannot acquire writer lock: {error}"))
            })?;
            let mut line =
                serde_json::to_vec(&header).map_err(|error| SessionError::new(&path, error))?;
            line.push(b'\n');
            file.write_all(&line)
                .and_then(|()| file.sync_all())
                .map_err(|error| SessionError::new(&path, error))?;
            #[cfg(unix)]
            File::open(&directory)
                .and_then(|file| file.sync_all())
                .map_err(|error| SessionError::new(&directory, error))?;
            Ok(line.len() as u64)
        })();
        let len = match result {
            Ok(len) => len,
            Err(error) => {
                drop(file);
                fs::remove_file(&path).map_err(|cleanup| {
                    SessionError::new(
                        &path,
                        format!("{error}; failed to remove incomplete session: {cleanup}"),
                    )
                })?;
                return Err(error);
            }
        };
        Ok(Self {
            path,
            file,
            writable: true,
            header,
            entries: Vec::new(),
            lines: Vec::new(),
            by_id: HashMap::new(),
            leaf: None,
            diagnostics: Vec::new(),
            file_len: len,
            valid_len: len,
            needs_newline: false,
            write_failed: false,
        })
    }

    pub(crate) fn load(
        path: &Path,
        writable: bool,
        cancel: &CancellationToken,
    ) -> Result<Self, SessionError> {
        check_cancel(path, cancel)?;
        let path = path
            .canonicalize()
            .map_err(|error| SessionError::new(path, error))?;
        let file = OpenOptions::new()
            .read(true)
            .append(writable)
            .open(&path)
            .map_err(|error| SessionError::new(&path, error))?;
        if writable {
            file.try_lock().map_err(|error| {
                SessionError::new(&path, format!("cannot acquire writer lock: {error}"))
            })?;
        }
        Self::load_file(path, file, writable, cancel)
    }

    pub(crate) fn load_file(
        path: PathBuf,
        file: File,
        writable: bool,
        cancel: &CancellationToken,
    ) -> Result<Self, SessionError> {
        let len = file
            .metadata()
            .map_err(|error| SessionError::new(&path, error))?
            .len();
        let mut reader = BufReader::new(&file);
        let mut line = Vec::new();
        reader
            .read_until(b'\n', &mut line)
            .map_err(|error| SessionError::new(&path, error))?;
        let header = parse_header(&path, &line)?;
        let valid_len = line.len() as u64;
        let needs_newline = !line.ends_with(b"\n");
        let mut store = Self {
            path: path.clone(),
            file: file
                .try_clone()
                .map_err(|error| SessionError::new(&path, error))?,
            writable,
            header,
            entries: Vec::new(),
            lines: Vec::new(),
            by_id: HashMap::new(),
            leaf: None,
            diagnostics: Vec::new(),
            file_len: len,
            valid_len,
            needs_newline,
            write_failed: false,
        };
        let mut number = 1;
        loop {
            check_cancel(&path, cancel)?;
            line.clear();
            let count = reader
                .read_until(b'\n', &mut line)
                .map_err(|error| SessionError::new(&path, error))?;
            if count == 0 {
                break;
            }
            number += 1;
            // Only malformed JSON at an unterminated EOF is a recoverable torn line.
            let value = match serde_json::from_slice::<serde_json::Value>(&line) {
                Ok(value) => value,
                Err(error) if !line.ends_with(b"\n") => {
                    store.diagnostics.push(Diagnostic {
                        line: number,
                        message: format!("ignored torn last line: {error}"),
                    });
                    break;
                }
                Err(error) => return Err(SessionError::at_line(&path, number, error)),
            };
            let entry: Entry = serde_json::from_value(value)
                .map_err(|error| SessionError::at_line(&path, number, error))?;
            store
                .validate_entry(&entry)
                .map_err(|error| SessionError::at_line(&path, number, error))?;
            store.valid_len += count as u64;
            store.needs_newline = !line.ends_with(b"\n");
            store.remember(entry, line.clone());
        }
        Ok(store)
    }

    pub(crate) fn write_entry(
        &mut self,
        entry: Entry,
        line: &[u8],
        cancel: &CancellationToken,
    ) -> Result<(), SessionError> {
        check_cancel(&self.path, cancel)?;
        if !self.writable {
            return Err(SessionError::new(&self.path, "session is read-only"));
        }
        if self.write_failed {
            return Err(SessionError::new(
                &self.path,
                "previous write failed; reopen before appending",
            ));
        }
        self.validate_entry(&entry)
            .map_err(|error| SessionError::new(&self.path, error))?;
        let metadata = self
            .file
            .metadata()
            .map_err(|error| SessionError::new(&self.path, error))?;
        if metadata.len() != self.file_len {
            return Err(SessionError::new(
                &self.path,
                "session changed outside this writer; reopen before appending",
            ));
        }
        // An I/O failure may leave a partial commit. Reject further writes from this snapshot.
        self.write_failed = true;
        if self.valid_len != self.file_len {
            self.file
                .set_len(self.valid_len)
                .map_err(|error| SessionError::new(&self.path, error))?;
        }
        let mut committed = Vec::new();
        if self.needs_newline {
            committed.push(b'\n');
        }
        committed.extend_from_slice(line);
        if !committed.ends_with(b"\n") {
            committed.push(b'\n');
        }
        self.file
            .write_all(&committed)
            .and_then(|()| self.file.sync_data())
            .map_err(|error| SessionError::new(&self.path, error))?;
        self.file_len = self.valid_len + committed.len() as u64;
        self.valid_len = self.file_len;
        self.needs_newline = false;
        self.write_failed = false;
        self.remember(entry, line.to_vec());
        Ok(())
    }
}

fn parse_header(path: &Path, line: &[u8]) -> Result<Header, SessionError> {
    let header: Header =
        serde_json::from_slice(line).map_err(|error| SessionError::at_line(path, 1, error))?;
    if header.format_version != FORMAT_VERSION || header.id.is_empty() || !header.cwd.is_absolute()
    {
        return Err(SessionError::at_line(
            path,
            1,
            "invalid header or unsupported format_version",
        ));
    }
    Ok(header)
}

fn canonical_cwd(cwd: &Path) -> Result<PathBuf, SessionError> {
    let canonical = cwd
        .canonicalize()
        .map_err(|error| SessionError::new(cwd, error))?;
    if !canonical.is_dir() {
        return Err(SessionError::new(cwd, "cwd must be a directory"));
    }
    Ok(canonical)
}

pub(crate) fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

pub(crate) fn new_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let time = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!(
        "{time:x}-{:x}-{:x}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::encode_cwd;

    #[test]
    fn shortened_name_uses_stable_fnv1a_of_full_bytes() {
        assert_eq!(
            encode_cwd(&[b'a'; 121]),
            format!("{}-ea8a536aa68d0114", "a".repeat(120))
        );
        let left = encode_cwd(&[b'a'; 122]);
        let right = encode_cwd(&[b'a'; 123]);
        assert_eq!(&left[..120], &right[..120]);
        assert_ne!(left, right);
    }

    #[test]
    fn shortening_preserves_escape_boundaries_and_short_names() {
        for length in [117, 118, 119, 120] {
            let mut bytes = vec![b'a'; length];
            bytes.push(0);
            let encoded = encode_cwd(&bytes);
            if length == 117 {
                assert_eq!(encoded, format!("{}%00", "a".repeat(length)));
            } else {
                let (prefix, hash) = encoded.rsplit_once('-').unwrap();
                assert_eq!(prefix, "a".repeat(length));
                assert_eq!(hash.len(), 16);
                assert!(hash.bytes().all(|byte| byte.is_ascii_hexdigit()));
            }
        }
        assert_eq!(encode_cwd(&[b'a'; 120]), "a".repeat(120));
        assert_eq!(encode_cwd(b"/a-b"), "%2Fa-b");
        assert_eq!(encode_cwd(b"/a/b"), "%2Fa%2Fb");
    }
}
