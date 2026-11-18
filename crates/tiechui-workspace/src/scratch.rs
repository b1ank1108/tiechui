use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::Result;
use crate::ports::{clean_path, resolve_existing_prefix};

const DEFAULT_SCRATCH_DIR: &str = "default";

fn scratch_root(home: &Path) -> Result<PathBuf> {
    let abs = if home.is_absolute() {
        home.to_path_buf()
    } else {
        std::path::absolute(home)?
    };
    Ok(clean_path(&abs).join("scratch"))
}

pub fn default_scratch_dir(home: &Path) -> Result<PathBuf> {
    let root = scratch_root(home)?;
    tiechui_config::private::create_dir_all(&root)?;
    let dir = root.join(DEFAULT_SCRATCH_DIR);
    tiechui_config::private::create_dir_all(&dir)?;
    Ok(dir)
}

pub fn list_scratch_dirs(home: &Path) -> Result<Vec<PathBuf>> {
    let root = scratch_root(home)?;
    let entries = match fs::read_dir(&root) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(Vec::new()),
        Err(err) => return Err(err.into()),
    };
    let mut dirs = Vec::new();
    for entry in entries {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            dirs.push(entry.path());
        }
    }
    dirs.sort();
    Ok(dirs)
}

pub fn is_scratch_dir(home: &Path, cwd: &Path) -> bool {
    let Ok(root) = scratch_root(home) else {
        return false;
    };
    let root_resolved = resolve_existing_prefix(&root);
    let cwd_resolved = resolve_existing_prefix(&clean_path(cwd));
    cwd_resolved
        .strip_prefix(&root_resolved)
        .is_ok_and(|rel| !rel.as_os_str().is_empty())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::fs;
    use std::path::Path;

    use super::{default_scratch_dir, is_scratch_dir, list_scratch_dirs};

    #[test]
    fn default_scratch_dir_is_shared_private_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");

        let concurrent: Vec<_> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| scope.spawn(|| default_scratch_dir(&home).unwrap()))
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let default = home.join("scratch/default");
        assert!(concurrent.iter().all(|path| *path == default));

        fs::write(default.join("note.txt"), "keep\n").unwrap();
        assert_eq!(default_scratch_dir(&home).unwrap(), default);
        assert_eq!(
            fs::read_to_string(default.join("note.txt")).unwrap(),
            "keep\n"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&default, fs::Permissions::from_mode(0o755)).unwrap();
            default_scratch_dir(&home).unwrap();
            for path in [home.join("scratch"), default] {
                let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o7777;
                assert_eq!(mode, 0o700, "{}", path.display());
            }
        }
    }

    #[test]
    fn list_scratch_dirs_skips_missing_root_without_creating_it_and_matches_children() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        fs::create_dir_all(&home).unwrap();

        let empty = list_scratch_dirs(&home).unwrap();
        assert!(empty.is_empty());
        assert!(!home.join("scratch").exists());
        assert!(!is_scratch_dir(&home, Path::new("/tmp/other")));
        assert!(!is_scratch_dir(&home, &home.join("scratch")));

        let default = default_scratch_dir(&home).unwrap();
        let legacy = home.join("scratch/2026-11-14-demo-aaaa1111");
        fs::create_dir(&legacy).unwrap();
        fs::write(home.join("scratch/ignore-file.txt"), "note\n").unwrap();
        let listed = list_scratch_dirs(&home).unwrap();
        assert_eq!(listed, vec![legacy.clone(), default.clone()]);
        assert!(is_scratch_dir(&home, &default));
        assert!(is_scratch_dir(&home, &legacy.canonicalize().unwrap()));
        assert!(!is_scratch_dir(&home, &home.join("worktrees/repo/wt")));
    }
}
