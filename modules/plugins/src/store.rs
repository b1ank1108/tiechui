use std::fs::{self, File, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tiechui_plugin_api::{CancellationToken, ToolError, serde_json};

use crate::{check, manifest};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Installation {
    pub source: PathBuf,
    pub content_hash: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub marketplace: Option<crate::marketplace::Pin>,
}

fn entries(directory: &Path, cancel: &CancellationToken) -> Result<Vec<fs::DirEntry>, ToolError> {
    let mut entries = Vec::new();
    for entry in fs::read_dir(directory)? {
        check(cancel)?;
        entries.push(entry?);
    }
    entries.sort_by_key(|e| e.file_name());
    Ok(entries)
}

pub(crate) fn walk(
    root: &Path,
    directory: &Path,
    cancel: &CancellationToken,
    fingerprint: bool,
    visit: &mut impl FnMut(&Path, bool) -> Result<(), ToolError>,
) -> Result<(), ToolError> {
    check(cancel)?;
    for entry in entries(directory, cancel)? {
        check(cancel)?;
        // Installation omits VCS metadata; approval still covers it if added later.
        if !fingerprint && entry.file_name() == ".git" {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if fingerprint
            && (matches!(name.as_ref(), "__pycache__" | ".venv")
                || name.ends_with(".pyc")
                || name.ends_with(".egg-info"))
        {
            continue;
        }
        let kind = entry.file_type()?;
        let path = entry.path();
        if !kind.is_file() && !kind.is_dir() {
            return Err(ToolError(format!(
                "{}: links and special files are not supported",
                path.display()
            )));
        }
        let relative = path
            .strip_prefix(root)
            .map_err(|e| ToolError(e.to_string()))?;
        visit(relative, kind.is_dir())?;
        if kind.is_dir() {
            walk(root, &path, cancel, fingerprint, visit)?;
        }
    }
    Ok(())
}

pub fn content_hash(root: &Path, cancel: &CancellationToken) -> Result<String, ToolError> {
    check(cancel)?;
    if !fs::symlink_metadata(root)?.file_type().is_dir() {
        return Err(ToolError("plugin root must be a directory".into()));
    }
    let mut hash = Sha256::new();
    walk(root, root, cancel, true, &mut |relative, directory| {
        let name = relative.as_os_str().as_encoded_bytes();
        hash.update([u8::from(directory)]);
        hash.update((name.len() as u64).to_le_bytes());
        hash.update(name);
        if !directory {
            let path = root.join(relative);
            let metadata = fs::metadata(&path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                hash.update((metadata.permissions().mode() & 0o111).to_le_bytes());
            }
            hash.update(metadata.len().to_le_bytes());
            let mut file = File::open(path)?.take(metadata.len().saturating_add(1));
            let mut buffer = [0; 64 * 1024];
            let mut read = 0_u64;
            loop {
                check(cancel)?;
                let count = file.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                hash.update(&buffer[..count]);
                read += count as u64;
            }
            if read != metadata.len() {
                return Err(ToolError("plugin file changed while fingerprinting".into()));
            }
        }
        Ok(())
    })?;
    Ok(format!("sha256:{:x}", hash.finalize()))
}

pub(crate) fn copy(
    source: &Path,
    target: &Path,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    walk(source, source, cancel, false, &mut |relative, directory| {
        let destination = target.join(relative);
        if directory {
            tiechui_plugin_api::private::create_dir_all(&destination)?;
        } else {
            let source = source.join(relative);
            let mut input = File::open(&source)?;
            let mut output = File::create(&destination)?;
            let mut buffer = [0; 64 * 1024];
            loop {
                check(cancel)?;
                let count = input.read(&mut buffer)?;
                if count == 0 {
                    break;
                }
                output.write_all(&buffer[..count])?;
            }
            fs::set_permissions(destination, fs::metadata(source)?.permissions())?;
        }
        Ok(())
    })
}

pub(crate) fn lock(home: &Path, cancel: &CancellationToken) -> Result<File, ToolError> {
    check(cancel)?;
    tiechui_plugin_api::private::create_dir_all(&home.join("plugins"))?;
    let lock = tiechui_plugin_api::private::open_lock(&home.join("plugins/.lock"))?;
    loop {
        check(cancel)?;
        match lock.try_lock() {
            Ok(()) => return Ok(lock),
            Err(TryLockError::WouldBlock) => {
                std::thread::sleep(std::time::Duration::from_millis(20))
            }
            Err(e) => return Err(ToolError(e.to_string())),
        }
    }
}

fn record_path(home: &Path, name: &str) -> PathBuf {
    home.join("plugins/.records").join(format!("{name}.json"))
}

pub(crate) fn read_installation(
    home: &Path,
    name: &str,
) -> Result<Option<Installation>, ToolError> {
    match fs::read(record_path(home, name)) {
        Ok(bytes) => serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| ToolError(e.to_string())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

pub fn add(source: &Path, home: &Path, cancel: &CancellationToken) -> Result<String, ToolError> {
    install(source, home, None, None, cancel)
}

pub(crate) fn install(
    source: &Path,
    home: &Path,
    pin: Option<crate::marketplace::Pin>,
    previous_hash: Option<&str>,
    cancel: &CancellationToken,
) -> Result<String, ToolError> {
    check(cancel)?;
    let source = source.canonicalize()?;
    let plugin = manifest::load(&source, false, None, cancel)?;
    let _lock = lock(home, cancel)?;
    let directory = home.join("plugins").canonicalize()?;
    if directory.starts_with(&source) {
        return Err(ToolError(
            "plugin source cannot contain the installation directory".into(),
        ));
    }
    let destination = directory.join(&plugin.name);
    if let Some(expected) = previous_hash {
        if content_hash(&destination, cancel)? != expected {
            return Err(ToolError("installed plugin changed during upgrade".into()));
        }
    } else if destination.try_exists()? || fs::symlink_metadata(&destination).is_ok() {
        return Err(ToolError(format!(
            "plugin {} is already installed",
            plugin.name
        )));
    }
    let stage = tempfile::Builder::new()
        .prefix(".install-")
        .tempdir_in(&directory)?;
    let staged_plugin = stage.path().join(&plugin.name);
    tiechui_plugin_api::private::create_dir_all(&staged_plugin)?;
    copy(&source, &staged_plugin, cancel)?;
    let copied = manifest::load(&staged_plugin, false, None, cancel)?;
    if copied.name != plugin.name {
        return Err(ToolError("plugin changed during installation".into()));
    }
    let installation = Installation {
        source: pin
            .as_ref()
            .map_or(source, |pin| PathBuf::from(&pin.source)),
        content_hash: content_hash(&staged_plugin, cancel)?,
        marketplace: pin,
    };
    let records = directory.join(".records");
    tiechui_plugin_api::private::create_dir_all(&records)?;
    let mut record = tempfile::NamedTempFile::new_in(records)?;
    serde_json::to_writer_pretty(&mut record, &installation)
        .map_err(|e| ToolError(e.to_string()))?;
    record.as_file().sync_all()?;
    check(cancel)?;
    if previous_hash.is_some_and(|hash| hash != installation.content_hash) {
        match fs::remove_file(
            home.join("plugin-grants")
                .join(format!("{}.json", plugin.name)),
        ) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
            _ => {}
        }
    }
    let backup = stage.path().join("previous");
    if previous_hash.is_some() {
        fs::rename(&destination, &backup)?;
    }
    if let Err(error) = fs::rename(&staged_plugin, &destination) {
        if previous_hash.is_some() {
            fs::rename(&backup, &destination)?;
        }
        return Err(error.into());
    }
    if let Err(e) = record.persist(record_path(home, &plugin.name)) {
        fs::remove_dir_all(&destination)?;
        if previous_hash.is_some() {
            fs::rename(backup, destination)?;
        }
        return Err(ToolError(e.to_string()));
    }
    Ok(plugin.name)
}

pub fn remove(name: &str, home: &Path, cancel: &CancellationToken) -> Result<(), ToolError> {
    if !manifest::valid_name(name) {
        return Err(ToolError("invalid or reserved plugin name".into()));
    }
    let _lock = lock(home, cancel)?;
    let path = home.join("plugins").join(name);
    if !fs::symlink_metadata(&path)?.file_type().is_dir() {
        return Err(ToolError("installed plugin must be a directory".into()));
    }
    check(cancel)?;
    fs::remove_dir_all(path)?;
    match fs::remove_file(record_path(home, name)) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    Ok(())
}
