//! Immutable file versions and an explicit, recoverable restore transaction.

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Component, Path, PathBuf},
    time::{Duration, Instant},
};
use tiechui_plugin_api::{
    CancellationToken, ToolError,
    protocol::{CheckpointStats, LineStats},
};

type Result<T> = std::result::Result<T, ToolError>;
const LIMIT: u64 = 20 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Version {
    pub hash: String,
    pub mode: u32,
}
type Files = BTreeMap<String, Option<Version>>;
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Manifest {
    version: u32,
    root: PathBuf,
    checkpoints: BTreeMap<String, Files>,
    head: Files,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub target: Files,
    pub current: Files,
    pub stats: CheckpointStats,
}

#[derive(Clone)]
pub struct Store {
    pub root: PathBuf,
    pub directory: PathBuf,
}

pub fn check(cancel: &CancellationToken) -> Result<()> {
    if cancel.is_cancelled() {
        Err(ToolError("Operation aborted".into()))
    } else {
        Ok(())
    }
}
fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn private_dir(path: &Path) -> Result<()> {
    tiechui_plugin_api::private::create_dir_all(path)?;
    Ok(())
}
#[cfg(unix)]
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::unix::fs::OpenOptionsExt;
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
}
#[cfg(windows)]
fn open_nofollow(path: &Path) -> std::io::Result<File> {
    use std::os::windows::fs::OpenOptionsExt;
    // FILE_FLAG_OPEN_REPARSE_POINT: a symlink opens as itself and fails the regular-file checks.
    OpenOptions::new()
        .read(true)
        .custom_flags(0x0020_0000)
        .open(path)
}
fn create_private(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    options.open(path)
}
#[cfg(unix)]
fn identity(stat: &fs::Metadata) -> (u64, u64) {
    use std::os::unix::fs::MetadataExt;
    (stat.dev(), stat.ino())
}
/// Stable Windows file IDs need nightly APIs; size and timestamps stand in.
#[cfg(windows)]
fn identity(stat: &fs::Metadata) -> (u64, u64) {
    use std::os::windows::fs::MetadataExt;
    (stat.creation_time(), stat.last_write_time())
}
#[cfg(unix)]
fn changed(stat: &fs::Metadata) -> (i64, i64, i64) {
    use std::os::unix::fs::MetadataExt;
    (stat.mtime(), stat.mtime_nsec(), stat.ctime_nsec())
}
#[cfg(windows)]
fn changed(stat: &fs::Metadata) -> (i64, i64, i64) {
    use std::os::windows::fs::MetadataExt;
    (
        stat.last_write_time() as i64,
        stat.creation_time() as i64,
        0,
    )
}
#[cfg(unix)]
fn links(stat: &fs::Metadata) -> u64 {
    std::os::unix::fs::MetadataExt::nlink(stat)
}
#[cfg(windows)]
fn links(_: &fs::Metadata) -> u64 {
    1
}
#[cfg(unix)]
fn mode(stat: &fs::Metadata) -> u32 {
    std::os::unix::fs::MetadataExt::mode(stat) & 0o777
}
#[cfg(windows)]
fn mode(stat: &fs::Metadata) -> u32 {
    if stat.permissions().readonly() {
        0o444
    } else {
        0o666
    }
}
#[cfg(unix)]
fn set_mode(file: &File, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    file.set_permissions(fs::Permissions::from_mode(mode))
}
#[cfg(windows)]
fn set_mode(file: &File, mode: u32) -> std::io::Result<()> {
    let mut permissions = file.metadata()?.permissions();
    permissions.set_readonly(mode & 0o222 == 0);
    file.set_permissions(permissions)
}
fn read_json<T: for<'a> Deserialize<'a>>(path: &Path) -> Result<T> {
    serde_json::from_reader(File::open(path)?).map_err(|e| ToolError(e.to_string()))
}

impl Store {
    pub fn new(base: &Path, root: &Path, session: &str) -> Result<Self> {
        let root = fs::canonicalize(root)?;
        let directory = base
            .join(&hash(root.as_os_str().as_encoded_bytes())[..24])
            .join(&hash(session.as_bytes())[..24]);
        Ok(Self { root, directory })
    }
    fn workspace(&self) -> &Path {
        self.directory.parent().unwrap_or(&self.directory)
    }
    pub fn atomic(&self, name: &str, data: &impl Serialize) -> Result<()> {
        private_dir(&self.directory)?;
        let mut file = tempfile::NamedTempFile::new_in(&self.directory)?;
        serde_json::to_writer(&mut file, data).map_err(|e| ToolError(e.to_string()))?;
        file.as_file().sync_all()?;
        file.persist(self.directory.join(name))
            .map_err(|e| ToolError(e.to_string()))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    fn load(&self) -> Result<Manifest> {
        let path = self.directory.join("manifest.json");
        match read_json::<Manifest>(&path) {
            Ok(data) if matches!(data.version, 1 | 2) && data.root == self.root => {
                Ok(Manifest { version: 2, ..data })
            }
            Ok(_) => Err(ToolError("Invalid rewind manifest".into())),
            Err(_) if !path.try_exists()? => Ok(Manifest {
                version: 2,
                root: self.root.clone(),
                checkpoints: BTreeMap::new(),
                head: BTreeMap::new(),
            }),
            Err(error) => Err(error),
        }
    }
    fn lock(&self, cancel: &CancellationToken) -> Result<File> {
        private_dir(self.workspace())?;
        // The OS releases this workspace lock after a crash; file leases remain explicit.
        let file = tiechui_plugin_api::private::open_lock(&self.workspace().join("writer.lock"))?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            check(cancel)?;
            if file.try_lock().is_ok() {
                return Ok(file);
            }
            if Instant::now() >= deadline {
                return Err(ToolError(
                    "Rewind storage is locked. Another process may be using this session.".into(),
                ));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
    pub fn pending(&self) -> Result<bool> {
        Ok(self.directory.join("transaction.json").try_exists()?)
    }
    pub fn file_key(&self, path: &Path, cwd: &Path) -> Result<String> {
        // A session's cwd may itself be an alias. Keep checking links below it.
        let path = path
            .strip_prefix(cwd)
            .map_or_else(|_| path.to_owned(), |relative| self.root.join(relative));
        #[cfg(target_os = "macos")]
        let path = {
            let mut path = path;
            // macOS exposes these system directories through root-level symlinks.
            for alias in ["/tmp", "/var", "/etc"] {
                if let Ok(relative) = path.strip_prefix(alias) {
                    path = fs::canonicalize(alias)?.join(relative);
                    break;
                }
            }
            path
        };
        path.strip_prefix(&self.root)
            .unwrap_or(&path)
            .to_str()
            .map(str::to_owned)
            .ok_or_else(|| ToolError("Invalid UTF-8 file path".into()))
    }
    pub fn safe_path(&self, file: &str) -> Result<PathBuf> {
        let path = Path::new(file);
        if path.file_name().is_none()
            || path
                .components()
                .any(|part| !matches!(part, Component::RootDir | Component::Normal(_)))
            || path.components().any(|part| {
                matches!(
                    part.as_os_str().to_str(),
                    Some(".git" | ".pi" | ".tiechui" | ".agents" | ".claude")
                )
            })
        {
            return Err(ToolError(format!("Unsafe rewind path: {file}")));
        }
        if fs::canonicalize(&self.root)? != self.root {
            return Err(ToolError("Workspace root changed".into()));
        }
        let full = self.root.join(path);
        let mut parent = full.parent();
        while let Some(path) = parent.filter(|path| *path != self.root) {
            match fs::symlink_metadata(path) {
                Ok(stat) if stat.is_dir() && !stat.file_type().is_symlink() => {}
                Ok(_) => return Err(ToolError(format!("Linked/non-directory parent: {file}"))),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            parent = path.parent();
        }
        Ok(full)
    }
    pub fn capture(&self, file: &str, cancel: &CancellationToken) -> Result<Option<Version>> {
        check(cancel)?;
        let full = self.safe_path(file)?;
        let named = match fs::symlink_metadata(&full) {
            Ok(stat) => stat,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        if !named.is_file() || named.file_type().is_symlink() || links(&named) != 1 {
            return Err(ToolError(format!(
                "Linked/non-regular file cannot be checkpointed: {file}"
            )));
        }
        if named.len() > LIMIT {
            return Err(ToolError(format!(
                "File exceeds 20 MiB checkpoint limit: {file}"
            )));
        }
        let mut opened = open_nofollow(&full)?;
        let stat = opened.metadata()?;
        if identity(&stat) != identity(&named) || links(&stat) != 1 {
            return Err(ToolError(format!("File changed during checkpoint: {file}")));
        }
        let mut bytes = Vec::new();
        let mut chunk = [0; 65536];
        loop {
            check(cancel)?;
            let n = opened.read(&mut chunk)?;
            if n == 0 {
                break;
            }
            bytes.extend_from_slice(&chunk[..n]);
            if bytes.len() as u64 > LIMIT {
                return Err(ToolError(format!(
                    "File exceeds 20 MiB checkpoint limit: {file}"
                )));
            }
        }
        let after = opened.metadata()?;
        if stat.len() != after.len() || changed(&stat) != changed(&after) {
            return Err(ToolError(format!("File changed during checkpoint: {file}")));
        }
        let version = Version {
            hash: hash(&bytes),
            mode: mode(&stat),
        };
        private_dir(&self.directory)?;
        private_dir(&self.directory.join("blobs"))?;
        let path = self.directory.join("blobs").join(&version.hash);
        if !path.try_exists()? {
            let mut blob = tempfile::NamedTempFile::new_in(self.directory.join("blobs"))?;
            blob.write_all(&bytes)?;
            blob.as_file().sync_all()?;
            match blob.persist_noclobber(&path) {
                Ok(_) => {}
                Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(ToolError(error.to_string())),
            }
        }
        self.bytes(&Some(version.clone()))?;
        Ok(Some(version))
    }
    pub fn bytes(&self, version: &Option<Version>) -> Result<Vec<u8>> {
        let Some(version) = version else {
            return Ok(vec![]);
        };
        if version.hash.len() != 64
            || !version
                .hash
                .bytes()
                .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        {
            return Err(ToolError("Invalid backup hash".into()));
        }
        let mut file = open_nofollow(&self.directory.join("blobs").join(&version.hash))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || links(&metadata) != 1 || metadata.len() > LIMIT {
            return Err(ToolError("Invalid rewind backup".into()));
        }
        let mut bytes = vec![];
        file.read_to_end(&mut bytes)?;
        if hash(&bytes) != version.hash {
            return Err(ToolError("Corrupt rewind backup".into()));
        }
        Ok(bytes)
    }
    fn lease(&self, file: &str) -> PathBuf {
        self.workspace()
            .join(format!("lease-{}", hash(file.as_bytes())))
    }
    pub fn before(&self, anchor: &str, file: &str, cancel: &CancellationToken) -> Result<()> {
        let _lock = self.lock(cancel)?;
        if self.pending()? {
            return Err(ToolError(
                "An incomplete rewind needs recovery; run /rewind before more edits.".into(),
            ));
        }
        self.safe_path(file)?;
        let mut lease = create_private(&self.lease(file))?;
        serde_json::to_writer(&mut lease, &(std::process::id(), &self.directory, file))
            .map_err(|e| ToolError(e.to_string()))?;
        lease.sync_all()?;
        let result = (|| {
            let mut data = self.load()?;
            let checkpoint = data.checkpoints.entry(anchor.into()).or_default();
            if !checkpoint.contains_key(file) {
                checkpoint.insert(file.into(), self.capture(file, cancel)?);
            }
            self.atomic("manifest.json", &data)
        })();
        if result.is_err() {
            fs::remove_file(self.lease(file))?;
        }
        result
    }
    pub fn after(&self, file: &str, cancel: &CancellationToken) -> Result<()> {
        let _lock = self.lock(cancel)?;
        let claim: (u32, PathBuf, String) = read_json(&self.lease(file))?;
        if claim != (std::process::id(), self.directory.clone(), file.into()) {
            return Err(ToolError(
                "Checkpoint lease belongs to another writer".into(),
            ));
        }
        let mut data = self.load()?;
        data.head.insert(file.into(), self.capture(file, cancel)?);
        self.atomic("manifest.json", &data)?;
        fs::remove_file(self.lease(file))?;
        Ok(())
    }
    fn no_writers(&self) -> Result<()> {
        if self.workspace().try_exists()?
            && fs::read_dir(self.workspace())?.any(|entry| {
                entry.map_or(true, |entry| {
                    entry.file_name().to_string_lossy().starts_with("lease-")
                })
            })
        {
            return Err(ToolError("Another edit is active or interrupted in this workspace. Stop all editing sessions before rewind.".into()));
        }
        Ok(())
    }
    pub fn plan(&self, ids: &[String], cancel: &CancellationToken) -> Result<Plan> {
        let data = self.load()?;
        let mut target = Files::new();
        for id in ids {
            check(cancel)?;
            if let Some(files) = data.checkpoints.get(id) {
                for (file, version) in files {
                    target
                        .entry(file.clone())
                        .or_insert_with(|| version.clone());
                }
            }
        }
        let mut current = Files::new();
        let mut stats = CheckpointStats::default();
        let deadline = Instant::now() + Duration::from_millis(500);
        for file in target.keys().cloned().collect::<Vec<_>>() {
            let now = self.capture(&file, cancel)?;
            if data.head.get(&file) != Some(&now) {
                return Err(ToolError(format!(
                    "External/concurrent modification detected: {file}. Rewind refuses to overwrite it."
                )));
            }
            let before = &target[&file];
            let before_bytes = self.bytes(before)?;
            if before == &now {
                target.remove(&file);
                continue;
            }
            let counts = if Instant::now() < deadline {
                line_stats(&before_bytes, &self.bytes(&now)?)
            } else {
                None
            };
            stats.files.insert(file.clone(), counts);
            current.insert(file, now);
        }
        Ok(Plan {
            target,
            current,
            stats,
        })
    }
    fn put(
        &self,
        file: &str,
        version: &Option<Version>,
        expected: &[Option<Version>],
        cancel: &CancellationToken,
    ) -> Result<()> {
        let bytes = self.bytes(version)?;
        let path = self.safe_path(file)?;
        if !expected.contains(&self.capture(file, cancel)?) {
            return Err(ToolError(format!("File changed before restore: {file}")));
        }
        if let Some(version) = version {
            let parent = path
                .parent()
                .ok_or_else(|| ToolError("Invalid restore parent".into()))?;
            fs::create_dir_all(parent)?;
            self.safe_path(file)?;
            let mut temp = tempfile::NamedTempFile::new_in(parent)?;
            temp.write_all(&bytes)?;
            set_mode(temp.as_file(), version.mode)?;
            temp.as_file().sync_all()?;
            check(cancel)?;
            if !expected.contains(&self.capture(file, cancel)?) {
                return Err(ToolError(format!("File changed before restore: {file}")));
            }
            self.safe_path(file)?;
            temp.persist(&path).map_err(|e| ToolError(e.to_string()))?;
            File::open(parent)?.sync_all()?;
        } else {
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        }
        Ok(())
    }
    /// The caller retains this lock until navigation and finish have settled.
    pub fn restore(&self, plan: &Plan, cancel: &CancellationToken) -> Result<File> {
        if plan.current.keys().ne(plan.target.keys()) {
            return Err(ToolError("Invalid rewind plan".into()));
        }
        let lock = self.lock(cancel)?;
        self.no_writers()?;
        if self.pending()? {
            return Err(ToolError(
                "An incomplete rewind needs recovery; run /rewind.".into(),
            ));
        }
        for (file, current) in &plan.current {
            if self.capture(file, cancel)? != *current {
                return Err(ToolError(format!("File changed since preview: {file}")));
            }
            self.bytes(&plan.target[file])?;
        }
        self.atomic("transaction.json", plan)?;
        for (file, target) in &plan.target {
            self.put(file, target, &[plan.current[file].clone()], cancel)?;
        }
        Ok(lock)
    }
    pub fn finish(&self, files: &Files) -> Result<()> {
        let mut data = self.load()?;
        data.head.extend(files.clone());
        self.atomic("manifest.json", &data)?;
        fs::remove_file(self.directory.join("transaction.json"))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
    pub fn recover(&self, cancel: &CancellationToken) -> Result<()> {
        let _lock = self.lock(cancel)?;
        self.no_writers()?;
        let plan: Plan = read_json(&self.directory.join("transaction.json"))?;
        if plan.current.keys().ne(plan.target.keys()) {
            return Err(ToolError("Invalid rewind journal".into()));
        }
        for (file, previous) in &plan.current {
            let now = self.capture(file, cancel)?;
            if now != *previous && now != plan.target[file] {
                return Err(ToolError(format!(
                    "File changed after interrupted rewind: {file}"
                )));
            }
            self.bytes(previous)?;
        }
        for (file, previous) in &plan.current {
            self.put(
                file,
                previous,
                &[previous.clone(), plan.target[file].clone()],
                cancel,
            )?;
        }
        self.finish(&plan.current)
    }
    pub fn inherit(&self, source: &Self, ids: &[String], cancel: &CancellationToken) -> Result<()> {
        if self.root != source.root || self.directory == source.directory {
            return Ok(());
        }
        let _lock = self.lock(cancel)?;
        self.no_writers()?;
        if self.pending()? || source.pending()? {
            return Err(ToolError(
                "Recover the interrupted rewind before inheriting checkpoints.".into(),
            ));
        }
        let mut data = self.load()?;
        if !data.checkpoints.is_empty() {
            return Ok(());
        }
        let parent = source.load()?;
        for id in ids {
            check(cancel)?;
            if let Some(checkpoint) = parent.checkpoints.get(id) {
                for (file, before) in checkpoint {
                    self.safe_path(file)?;
                    if let Some(head) = parent.head.get(file) {
                        data.head.insert(file.clone(), head.clone());
                    }
                    for version in [Some(before), parent.head.get(file)]
                        .into_iter()
                        .flatten()
                        .flatten()
                    {
                        let bytes = source.bytes(&Some(version.clone()))?;
                        private_dir(&self.directory.join("blobs"))?;
                        let path = self.directory.join("blobs").join(&version.hash);
                        if !path.try_exists()? {
                            let mut blob = create_private(&path)?;
                            blob.write_all(&bytes)?;
                            blob.sync_all()?;
                        }
                        self.bytes(&Some(version.clone()))?;
                    }
                }
                data.checkpoints.insert(id.clone(), checkpoint.clone());
            }
        }
        self.atomic("manifest.json", &data)
    }
}

pub fn line_stats(before: &[u8], current: &[u8]) -> Option<LineStats> {
    if [before, current]
        .iter()
        .any(|bytes| bytes.len() > 1024 * 1024 || bytes.contains(&0))
    {
        return None;
    }
    let before = std::str::from_utf8(before).ok()?;
    let current = std::str::from_utf8(current).ok()?;
    let started = Instant::now();
    let diff = similar::TextDiff::configure()
        .timeout(Duration::from_millis(50))
        .diff_lines(before, current);
    if started.elapsed() >= Duration::from_millis(50) {
        return None;
    }
    let mut stats = LineStats::default();
    for change in diff.iter_all_changes() {
        match change.tag() {
            similar::ChangeTag::Insert => stats.added += 1,
            similar::ChangeTag::Delete => stats.removed += 1,
            _ => {}
        }
    }
    (stats.added + stats.removed <= 5000).then_some(stats)
}
