use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::{Component, Path, PathBuf};

use ignore::WalkBuilder;
use sha2::{Digest, Sha256};
use tiechui_protocol::workspace::{FileContent, FileEntry, FileTree, WriteResult};

use crate::{Result, fail};

const MAX_ENTRIES: usize = 20_000;
const MAX_READ: u64 = 10 << 20;

/// `root` joined with a relative path that stays inside it; empty names the root.
pub(crate) fn resolve(root: &Path, path: &str) -> Result<PathBuf> {
    if Path::new(path)
        .components()
        .any(|part| !matches!(part, Component::Normal(_)))
    {
        return fail(format!("{path} is not a relative path inside the project"));
    }
    Ok(root.join(path))
}

fn relative(root: &Path, path: &Path) -> Option<String> {
    let relative = path.strip_prefix(root).ok()?.to_str()?;
    (!relative.is_empty()).then(|| relative.replace('\\', "/"))
}

pub(crate) fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Binary files carry a NUL early on or are not UTF-8.
pub(crate) fn text(bytes: Vec<u8>) -> Option<String> {
    if bytes[..bytes.len().min(8192)].contains(&0) {
        return None;
    }
    String::from_utf8(bytes).ok()
}

pub(crate) fn is_upload_temp_name(name: &std::ffi::OsStr) -> bool {
    name.to_str()
        .is_some_and(|s| s.starts_with(".tiechui-upload-"))
}

pub(crate) fn is_excluded_rel_path(rel: &Path) -> bool {
    if rel
        .file_name()
        .is_some_and(|name| name == ".git" || is_upload_temp_name(name))
    {
        return true;
    }
    let mut comps = rel.components();
    matches!(
        (comps.next(), comps.next()),
        (Some(Component::Normal(first)), Some(Component::Normal(second)))
            if first.to_string_lossy().eq_ignore_ascii_case(".tiechui")
                && second.to_string_lossy().eq_ignore_ascii_case("worktrees")
    )
}

pub(crate) fn excluded_managed_worktrees(home: Option<&Path>, root: &Path) -> Vec<PathBuf> {
    let Some(home) = home else {
        return Vec::new();
    };
    let root_res = crate::ports::resolve_existing_prefix(root);
    crate::worktree::project_worktree_paths(home, root)
        .into_iter()
        .map(|p| crate::ports::resolve_existing_prefix(&p))
        .filter(|wt_res| {
            !crate::win_path::paths_equal(wt_res, &root_res)
                && crate::win_path::path_starts_with(wt_res, &root_res)
        })
        .collect()
}

pub(crate) fn should_skip_workspace_entry(
    root: &Path,
    entry_path: &Path,
    excluded_worktrees: &[PathBuf],
) -> bool {
    if entry_path
        .file_name()
        .is_some_and(|name| name == ".git" || is_upload_temp_name(name))
    {
        return true;
    }
    if let Ok(rel) = entry_path.strip_prefix(root)
        && is_excluded_rel_path(rel)
    {
        return true;
    }
    if !excluded_worktrees.is_empty() {
        let candidate = crate::ports::resolve_existing_prefix(entry_path);
        if excluded_worktrees
            .iter()
            .any(|wt| crate::win_path::path_starts_with(&candidate, wt))
        {
            return true;
        }
    }
    false
}

pub(crate) fn linked_worktree_info_exclude(root: &Path) -> Option<PathBuf> {
    let dot_git = root.join(".git");
    if !dot_git.is_file() {
        return None;
    }
    let text = std::fs::read_to_string(&dot_git).ok()?;
    let raw_gitdir = text.trim().strip_prefix("gitdir:")?.trim();
    let git_dir = if Path::new(raw_gitdir).is_absolute() {
        PathBuf::from(raw_gitdir)
    } else {
        root.join(raw_gitdir)
    };
    let common_dir = match std::fs::read_to_string(git_dir.join("commondir")) {
        Ok(raw_common) => {
            let trimmed = raw_common.trim();
            if Path::new(trimmed).is_absolute() {
                PathBuf::from(trimmed)
            } else {
                git_dir.join(trimmed)
            }
        }
        Err(_) => git_dir,
    };
    let exclude = common_dir.join("info/exclude");
    exclude.is_file().then_some(exclude)
}

pub(crate) fn is_worktrees_holder(root: &Path, path: &Path) -> bool {
    path == root.join(".tiechui") && path.join("worktrees").exists()
}

/// Walks `root` without the entries `should_skip_workspace_entry` hides.
pub(crate) fn workspace_walk(root: &Path, excluded_wt: &[PathBuf]) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder.hidden(false);
    if root.join(".git").is_file() {
        builder.parents(false).require_git(false);
        if let Some(exclude) = linked_worktree_info_exclude(root) {
            let _ = builder.add_ignore(exclude);
        }
    }
    let filter_root = root.to_owned();
    let filter_wt = excluded_wt.to_vec();
    builder.filter_entry(move |entry| {
        entry.depth() == 0 || !should_skip_workspace_entry(&filter_root, entry.path(), &filter_wt)
    });
    builder
}

/// Entries outside `.gitignore`, plus each listed directory's ignored entries, which are not
/// descended into. `.git` and managed `.tiechui/worktrees` checkouts are left out.
pub fn tree(root: &Path) -> Result<FileTree> {
    tree_with_home(None, root)
}

pub fn tree_with_home(home: Option<&Path>, root: &Path) -> Result<FileTree> {
    let mut entries = Vec::new();
    let mut listed = HashSet::new();
    let mut dirs = vec![root.to_owned()];
    let excluded_wt = excluded_managed_worktrees(home, root);
    let walk = workspace_walk(root, &excluded_wt).build();
    for entry in walk {
        let entry = entry?;
        let Some(path) = relative(root, entry.path()) else {
            continue;
        };
        let dir = entry.file_type().is_some_and(|kind| kind.is_dir());
        if dir {
            dirs.push(entry.path().to_owned());
        }
        listed.insert(entry.into_path());
        entries.push(FileEntry {
            path,
            dir,
            ignored: false,
        });
        if entries.len() >= MAX_ENTRIES {
            break;
        }
    }
    for dir in dirs {
        for child in std::fs::read_dir(&dir)? {
            let child = child?;
            let path = child.path();
            if should_skip_workspace_entry(root, &path, &excluded_wt) || listed.contains(&path) {
                continue;
            }
            if let Some(relative) = relative(root, &path) {
                entries.push(FileEntry {
                    path: relative,
                    dir: path.is_dir(),
                    ignored: true,
                });
            }
        }
    }
    hide_worktrees_holder(root, &mut entries);
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    let truncated = entries.len() >= MAX_ENTRIES;
    entries.truncate(MAX_ENTRIES);
    Ok(FileTree { entries, truncated })
}

fn hide_worktrees_holder(root: &Path, entries: &mut Vec<FileEntry>) {
    if is_worktrees_holder(root, &root.join(".tiechui"))
        && !entries.iter().any(|e| e.path.starts_with(".tiechui/"))
    {
        entries.retain(|e| e.path != ".tiechui");
    }
}

/// The entries of a directory inside an ignored one, all ignored.
pub fn list(root: &Path, path: &str) -> Result<Vec<FileEntry>> {
    let mut entries = Vec::new();
    for child in std::fs::read_dir(resolve(root, path)?)? {
        let child = child?;
        if let Some(relative) = relative(root, &child.path()) {
            entries.push(FileEntry {
                path: relative,
                dir: child.path().is_dir(),
                ignored: true,
            });
        }
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(entries)
}

pub fn read(root: &Path, path: &str) -> Result<FileContent> {
    let path = resolve(root, path)?;
    let size = std::fs::metadata(&path)?.len();
    if size > MAX_READ {
        return fail("The file exceeds 10 MiB");
    }
    let bytes = std::fs::read(&path)?;
    Ok(FileContent {
        hash: hash(&bytes),
        size: bytes.len() as u64,
        text: text(bytes),
    })
}

pub fn write(root: &Path, path: &str, text: &str, expected: Option<&str>) -> Result<WriteResult> {
    let path = resolve(root, path)?;
    if let Some(expected) = expected {
        let current = match std::fs::read(&path) {
            Ok(bytes) => hash(&bytes),
            Err(error) if error.kind() == ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        if current != expected {
            return fail("The file changed on disk");
        }
    }
    std::fs::write(&path, text)?;
    Ok(WriteResult {
        hash: hash(text.as_bytes()),
    })
}

pub fn create(root: &Path, path: &str) -> Result<()> {
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(resolve(root, path)?)?;
    Ok(())
}

pub fn mkdir(root: &Path, path: &str) -> Result<()> {
    Ok(std::fs::create_dir(resolve(root, path)?)?)
}

/// Moves the file or directory to the system trash.
pub fn delete(root: &Path, path: &str) -> Result<()> {
    let path = resolve(root, path)?;
    if path == root {
        return fail("The project root cannot be deleted");
    }
    #[cfg(target_os = "macos")]
    {
        use trash::macos::{DeleteMethod, TrashContextExtMacos};
        let mut context = trash::TrashContext::default();
        // Finder would ask for permission to send it Apple events.
        context.set_delete_method(DeleteMethod::NsFileManager);
        Ok(context.delete(path)?)
    }
    #[cfg(not(target_os = "macos"))]
    Ok(trash::delete(path)?)
}

pub fn rename(root: &Path, from: &str, to: &str) -> Result<()> {
    let (from, to) = (resolve(root, from)?, resolve(root, to)?);
    if to.symlink_metadata().is_ok() {
        return fail(format!("{} exists", to.display()));
    }
    Ok(std::fs::rename(from, to)?)
}
