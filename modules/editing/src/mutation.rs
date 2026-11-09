use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use tiechui_plugin_api::model::ToolCall;
use tiechui_plugin_api::tools::{ToolContext, ToolError, ToolOutput, exec};
use tokio_util::sync::CancellationToken;

use crate::{
    EditingState, check, edit, resolve,
    state::{stamp, version},
    string,
};

/// Opens without following a final symlink and without blocking on FIFOs.
pub(crate) fn open_nofollow(options: &mut OpenOptions) -> &mut OpenOptions {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_FLAG_OPEN_REPARSE_POINT opens a symlink itself, which `is_file` then rejects.
        options.custom_flags(0x0020_0000);
    }
    options
}
struct Snapshot {
    requested: PathBuf,
    target: PathBuf,
    entry: Option<Vec<i128>>,
    identity: Option<Vec<i128>>,
    bytes: Option<Vec<u8>>,
}
fn changed(path: &Path) -> ToolError {
    ToolError(format!(
        "File changed during the operation: {}. Nothing was written; read the current file and retry.",
        path.display()
    ))
}
fn read_all(file: &mut File, cancel: &CancellationToken) -> Result<Vec<u8>, ToolError> {
    let mut bytes = Vec::new();
    let mut chunk = [0; 65536];
    loop {
        check(cancel)?;
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n]);
    }
    check(cancel)?;
    Ok(bytes)
}
fn capture(requested: &Path, cancel: &CancellationToken) -> Result<Snapshot, ToolError> {
    check(cancel)?;
    let entry = match fs::symlink_metadata(requested) {
        Ok(entry) => entry,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let parent = requested
                .parent()
                .ok_or_else(|| ToolError("Missing file parent".into()))?;
            let target = fs::canonicalize(parent)?.join(requested.file_name().unwrap_or_default());
            return Ok(Snapshot {
                requested: requested.into(),
                target,
                entry: None,
                identity: None,
                bytes: None,
            });
        }
        Err(e) => return Err(e.into()),
    };
    let target = fs::canonicalize(requested)?;
    if !fs::symlink_metadata(&target)?.is_file() {
        return Err(ToolError(format!(
            "Only regular files can be modified: {}",
            requested.display()
        )));
    }
    let mut file = open_nofollow(OpenOptions::new().read(true)).open(&target)?;
    let identity = file.metadata()?;
    if !identity.is_file() {
        return Err(ToolError(format!(
            "Only regular files can be modified: {}",
            requested.display()
        )));
    }
    let bytes = read_all(&mut file, cancel)?;
    if stamp(&identity) != stamp(&file.metadata()?)
        || stamp(&entry) != stamp(&fs::symlink_metadata(requested)?)
        || target != fs::canonicalize(requested)?
        || stamp(&identity) != stamp(&fs::symlink_metadata(&target)?)
    {
        return Err(changed(requested));
    }
    Ok(Snapshot {
        requested: requested.into(),
        target,
        entry: Some(stamp(&entry)),
        identity: Some(stamp(&identity)),
        bytes: Some(bytes),
    })
}
fn verify(snapshot: &Snapshot, cancel: &CancellationToken) -> Result<(), ToolError> {
    let current = capture(&snapshot.requested, cancel).map_err(|_| {
        check(cancel)
            .err()
            .unwrap_or_else(|| changed(&snapshot.requested))
    })?;
    if snapshot.target != current.target
        || snapshot.entry != current.entry
        || snapshot.identity != current.identity
        || snapshot.bytes != current.bytes
    {
        return Err(changed(&snapshot.requested));
    }
    check(cancel)
}
fn copy_command(cancel: &CancellationToken) -> Result<Option<(String, Vec<String>)>, ToolError> {
    if cfg!(windows) {
        // Rename keeps the new file's default ACL; edit the existing file in place instead.
        return Ok(None);
    }
    if cfg!(target_os = "macos") {
        return Ok(Some(("/bin/cp".into(), vec!["-p".into()])));
    }
    if cfg!(target_os = "linux") {
        let mut child = match Command::new("cp")
            .arg("--version")
            .env("LC_ALL", "C")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => child,
            Err(_) => return Ok(None),
        };
        let start = Instant::now();
        loop {
            if child.try_wait()?.is_some() {
                break;
            }
            if cancel.is_cancelled() || start.elapsed() > Duration::from_secs(2) {
                let _ = child.kill();
                child.wait()?;
                check(cancel)?;
                return Ok(None);
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        let output = child.wait_with_output()?;
        if String::from_utf8_lossy(&output.stdout).contains("GNU coreutils") {
            return Ok(Some((
                "cp".into(),
                vec!["--preserve=all".into(), "--".into()],
            )));
        }
    }
    Ok(None)
}
fn copy_metadata(
    command: &str,
    args: &[String],
    from: &Path,
    to: &Path,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    let mut command = Command::new(command);
    command
        .args(args)
        .arg(from)
        .arg(to)
        .env_remove("COPYFILE_DISABLE")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let (mut child, mut tree) = exec::Tree::spawn_std(&mut command)?;
    loop {
        if let Some(status) = child.try_wait()? {
            tree.disarm()?;
            if !status.success() {
                return Err(ToolError(format!(
                    "Cannot preserve file metadata: {status}"
                )));
            }
            return check(cancel);
        }
        if cancel.is_cancelled() {
            tree.interrupt()?;
            let until = Instant::now() + Duration::from_secs(1);
            while Instant::now() < until && child.try_wait()?.is_none() {
                std::thread::sleep(Duration::from_millis(5));
            }
            tree.kill()?;
            child.wait()?;
            return check(cancel);
        }
        std::thread::sleep(Duration::from_millis(5));
    }
}
type SyncDirectory<'a> = &'a dyn Fn(&Path) -> std::io::Result<()>;
#[derive(Default)]
struct Hooks<'a> {
    before_verify: Option<&'a dyn Fn()>,
    after_publish: Option<&'a dyn Fn()>,
    sync_directory: Option<SyncDirectory<'a>>,
}
fn publish(
    snapshot: &Snapshot,
    bytes: &[u8],
    cancel: &CancellationToken,
    hooks: Hooks<'_>,
) -> Result<(), ToolError> {
    check(cancel)?;
    if snapshot.identity.is_some() {
        writable(&snapshot.target)?;
    }
    let parent = snapshot
        .target
        .parent()
        .ok_or_else(|| ToolError("Missing file parent".into()))?;
    let copier = if snapshot.identity.is_some() {
        copy_command(cancel)?
    } else {
        None
    };
    let in_place = snapshot.identity.as_ref().is_some_and(|s| s[5] > 1)
        || (snapshot.identity.is_some() && copier.is_none());
    let mut published = false;
    let mut started = false;
    let mut staged = None;
    let result = (|| -> Result<(), ToolError> {
        if in_place {
            let mut file =
                open_nofollow(OpenOptions::new().read(true).write(true)).open(&snapshot.target)?;
            if let Some(hook) = hooks.before_verify {
                hook();
            }
            verify(snapshot, cancel)?;
            if snapshot.identity.as_ref() != Some(&stamp(&file.metadata()?)) {
                return Err(changed(&snapshot.requested));
            }
            check(cancel)?;
            started = true;
            file.write_all(bytes)?;
            file.set_len(bytes.len() as u64)?;
            published = true;
            if let Some(hook) = hooks.after_publish {
                hook();
            }
            file.sync_all()?;
        } else {
            let directory = tempfile::Builder::new()
                .prefix(".tc-write-")
                .tempdir_in(parent)?;
            let temporary = directory.path().join("content");
            staged = Some(directory);
            if let Some((command, args)) = &copier {
                copy_metadata(command, args, &snapshot.target, &temporary, cancel)?;
            }
            let mut options = OpenOptions::new();
            options.read(true).write(true);
            if copier.is_some() {
                open_nofollow(&mut options);
            } else {
                options.create_new(true);
            }
            let mut file = options.open(&temporary)?;
            file.write_all(bytes)?;
            file.set_len(bytes.len() as u64)?;
            check(cancel)?;
            #[cfg(unix)]
            if let Some(identity) = &snapshot.identity {
                restore_owner(&file, identity)?;
            }
            file.sync_all()?;
            drop(file);
            if let Some(hook) = hooks.before_verify {
                hook();
            }
            verify(snapshot, cancel)?;
            if snapshot.identity.is_some() {
                fs::rename(&temporary, &snapshot.target)?;
            } else if let Err(error) = fs::hard_link(&temporary, &snapshot.target) {
                if !hard_link_unsupported(&error) {
                    return Err(error.into());
                }
                check(cancel)?;
                let mut file = OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .open(&snapshot.target)?;
                started = true;
                file.write_all(bytes)?;
                published = true;
                file.sync_all()?;
            }
            published = true;
            if let Some(hook) = hooks.after_publish {
                hook();
            }
            if let Some(sync) = hooks.sync_directory {
                sync(parent)?;
            } else {
                sync_directory(parent)?;
            }
        }
        Ok(())
    })();
    let result=result.map_err(|error| if published { ToolError(format!("File was written to {}, but finalization failed: {error}. Verify the file before retrying.",snapshot.requested.display())) }
        else if started { ToolError(format!("Writing {} failed after the file may have changed: {error}. Verify the file before retrying.",snapshot.requested.display())) } else { error });
    if let Some(directory) = staged
        && let Err(error) = directory.close()
    {
        let state = if published {
            "File was written"
        } else if started {
            "File may have changed"
        } else {
            "File was not published"
        };
        return Err(ToolError(format!(
            "{state}; temporary cleanup failed: {error}"
        )));
    }
    result
}
#[cfg(unix)]
fn writable(path: &Path) -> Result<(), ToolError> {
    use std::os::unix::ffi::OsStrExt;
    let name = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|e| ToolError(e.to_string()))?;
    if unsafe { libc::access(name.as_ptr(), libc::W_OK) } != 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}
#[cfg(windows)]
fn writable(path: &Path) -> Result<(), ToolError> {
    OpenOptions::new().write(true).open(path)?;
    Ok(())
}
#[cfg(unix)]
fn restore_owner(file: &File, identity: &[i128]) -> Result<(), ToolError> {
    use std::os::fd::AsRawFd;
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let owner = file.metadata()?;
    if (owner.uid() != identity[7] as u32 || owner.gid() != identity[8] as u32)
        && unsafe { libc::fchown(file.as_raw_fd(), identity[7] as u32, identity[8] as u32) } != 0
    {
        return Err(std::io::Error::last_os_error().into());
    }
    file.set_permissions(fs::Permissions::from_mode(identity[6] as u32 & 0o7777))?;
    Ok(())
}
fn hard_link_unsupported(error: &std::io::Error) -> bool {
    #[cfg(unix)]
    return matches!(
        error.raw_os_error(),
        Some(libc::ENOTSUP | libc::ENOSYS | libc::EPERM)
    );
    // ERROR_INVALID_FUNCTION, ERROR_NOT_SUPPORTED, ERROR_ACCESS_DENIED
    #[cfg(windows)]
    return matches!(error.raw_os_error(), Some(1 | 50 | 5));
}
#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}
/// Windows cannot open a directory as a file; NTFS journals the rename itself.
#[cfg(windows)]
fn sync_directory(_: &Path) -> std::io::Result<()> {
    Ok(())
}
pub(crate) fn run(
    call: ToolCall,
    ctx: ToolContext,
    state: &EditingState,
) -> Result<ToolOutput, ToolError> {
    let input = call.arguments;
    let display = string(&input, "path")?;
    let path = resolve(display, &ctx.cwd)?;
    let is_edit = call.name == "edit";
    let edits = if is_edit {
        Some(edit::arguments(&input)?)
    } else {
        None
    };
    check(&ctx.cancel)?;
    if !is_edit {
        fs::create_dir_all(
            path.parent()
                .ok_or_else(|| ToolError("Missing parent".into()))?,
        )?;
    }
    let snapshot = capture(&path, &ctx.cancel).map_err(|e| {
        if is_edit && !path.exists() {
            ToolError(format!(
                "Could not edit file: {display}. Error code: ENOENT."
            ))
        } else {
            e
        }
    })?;
    let generation = state.generation();
    let views = if let Some(bytes) = &snapshot.bytes {
        state.check(&version(&path)?, &String::from_utf8_lossy(bytes))?
    } else {
        Vec::new()
    };
    let (bytes, mut output) = if let Some(edits) = edits {
        let count = edits.len();
        let prepared = edit::prepare(
            snapshot.bytes.as_deref().ok_or_else(|| {
                ToolError(format!(
                    "Could not edit file: {display}. Error code: ENOENT."
                ))
            })?,
            edits,
            display,
            &views,
        )?;
        let mut output = ToolOutput::text(format!(
            "Successfully replaced {count} block(s) in {display}.{}",
            prepared.recovery_notice
        ));
        output.details = Some(prepared.details);
        (prepared.bytes, output)
    } else {
        (
            string(&input, "content")?.as_bytes().to_vec(),
            ToolOutput::text(format!("Successfully wrote to {display}")),
        )
    };
    publish(&snapshot, &bytes, &ctx.cancel, Hooks::default())?;
    match version(&path) {
        Ok(version) => state.record(
            version,
            &String::from_utf8_lossy(&bytes),
            generation,
            true,
            None,
            !is_edit,
        ),
        Err(_) => state.clear(),
    }
    if ctx.cancel.is_cancelled()
        && let Some(tiechui_plugin_api::model::Content::Text { text, .. }) =
            output.content.first_mut()
    {
        *text += if is_edit {
            " Cancellation arrived after the file was written; the edit is complete."
        } else {
            ". Cancellation arrived after the file was written; the write is complete."
        };
    }
    Ok(output)
}

#[cfg(all(test, unix))]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::symlink;

    #[test]
    fn publication_rechecks_contents_identity_and_symlink_targets() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a");
        let cancel = CancellationToken::new();
        for identity_only in [false, true] {
            fs::write(&path, "one").unwrap();
            let snapshot = capture(&path, &cancel).unwrap();
            if identity_only {
                let replacement = directory.path().join("external");
                fs::write(&replacement, "one").unwrap();
                fs::rename(replacement, &path).unwrap();
            } else {
                fs::write(&path, "external").unwrap();
            }
            let error = publish(&snapshot, b"ours", &cancel, Hooks::default()).unwrap_err();
            assert!(error.0.contains("File changed during"));
            assert_eq!(
                fs::read_to_string(&path).unwrap(),
                if identity_only { "one" } else { "external" }
            );
        }
        let missing = directory.path().join("missing");
        let snapshot = capture(&missing, &cancel).unwrap();
        fs::write(&missing, "external").unwrap();
        assert!(publish(&snapshot, b"ours", &cancel, Hooks::default()).is_err());
        assert_eq!(fs::read_to_string(&missing).unwrap(), "external");
        let alias = directory.path().join("alias");
        symlink(&path, &alias).unwrap();
        let snapshot = capture(&alias, &cancel).unwrap();
        fs::remove_file(&alias).unwrap();
        symlink(&missing, &alias).unwrap();
        assert!(publish(&snapshot, b"ours", &cancel, Hooks::default()).is_err());
        assert_eq!(fs::read_to_string(&missing).unwrap(), "external");
        assert!(!fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".tc-write-")
        }));
    }

    #[test]
    fn cancellation_and_finalization_failures_report_the_actual_write_state() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a");
        for hardlink in [false, true] {
            fs::write(&path, "one").unwrap();
            if hardlink {
                fs::hard_link(&path, directory.path().join("hard")).unwrap();
            }
            for after in [false, true] {
                fs::write(&path, "one").unwrap();
                let cancel = CancellationToken::new();
                let snapshot = capture(&path, &cancel).unwrap();
                let abort = || cancel.cancel();
                let hooks = if after {
                    Hooks {
                        after_publish: Some(&abort),
                        ..Default::default()
                    }
                } else {
                    Hooks {
                        before_verify: Some(&abort),
                        ..Default::default()
                    }
                };
                let result = publish(&snapshot, b"ONE", &cancel, hooks);
                if after {
                    result.unwrap();
                } else {
                    assert_eq!(result.unwrap_err().0, "Operation aborted");
                }
                assert_eq!(
                    fs::read_to_string(&path).unwrap(),
                    if after { "ONE" } else { "one" }
                );
            }
        }
        fs::remove_file(directory.path().join("hard")).unwrap();
        let cancel = CancellationToken::new();
        let snapshot = capture(&path, &cancel).unwrap();
        let fail = |_: &Path| Err(std::io::Error::other("fixture sync failure"));
        let error = publish(
            &snapshot,
            b"complete",
            &cancel,
            Hooks {
                sync_directory: Some(&fail),
                ..Default::default()
            },
        )
        .unwrap_err();
        assert!(error.0.contains("File was written") && error.0.contains("finalization failed"));
        assert_eq!(fs::read_to_string(&path).unwrap(), "complete");
    }

    #[cfg(unix)]
    #[test]
    fn special_files_are_rejected_before_opening_them() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;
        let directory = tempfile::tempdir().unwrap();
        let fifo = directory.path().join("fifo");
        let name = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let error = capture(&fifo, &CancellationToken::new()).err().unwrap();
        assert!(error.0.contains("regular files"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn publication_preserves_macos_xattrs_and_acl() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("a");
        fs::write(&path, "one").unwrap();
        assert!(
            Command::new("/usr/bin/xattr")
                .args(["-w", "com.tiechui.fixture", "preserve"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        assert!(
            Command::new("/bin/chmod")
                .args(["+a", "everyone allow read"])
                .arg(&path)
                .status()
                .unwrap()
                .success()
        );
        let acl = || {
            String::from_utf8(
                Command::new("/bin/ls")
                    .arg("-lde")
                    .arg(&path)
                    .output()
                    .unwrap()
                    .stdout,
            )
            .unwrap()
            .lines()
            .skip(1)
            .map(str::to_owned)
            .collect::<Vec<_>>()
        };
        let before = acl();
        let cancel = CancellationToken::new();
        publish(
            &capture(&path, &cancel).unwrap(),
            b"ONE",
            &cancel,
            Hooks::default(),
        )
        .unwrap();
        assert_eq!(
            Command::new("/usr/bin/xattr")
                .args(["-p", "com.tiechui.fixture"])
                .arg(&path)
                .output()
                .unwrap()
                .stdout,
            b"preserve\n"
        );
        assert_eq!(acl(), before);
    }
}
