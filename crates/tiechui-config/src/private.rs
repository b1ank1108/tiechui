//! Private profile storage. Permission repairs operate on owned, opened inodes.

use std::fs::{self, File, OpenOptions};
use std::io;
use std::path::Path;

/// Creates missing parents privately and tightens the requested directory.
/// Existing ancestors are left alone: they may be outside the profile.
pub fn create_dir_all(path: &Path) -> io::Result<()> {
    let mut builder = fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        let directory = OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(path)?;
        tighten(&directory, 0o700)?;
    }
    Ok(())
}

/// Opens a persistent lock inode without truncating it or following a symlink.
pub fn open_lock(path: &Path) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(io::Error::other("Profile lock is not a regular file"));
    }
    #[cfg(unix)]
    tighten(&file, 0o600)?;
    Ok(file)
}

#[cfg(unix)]
fn tighten(file: &File, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let metadata = file.metadata()?;
    if metadata.uid() == unsafe { libc::geteuid() }
        && metadata.permissions().mode() & 0o7777 != mode
    {
        file.set_permissions(fs::Permissions::from_mode(mode))?;
    }
    Ok(())
}
