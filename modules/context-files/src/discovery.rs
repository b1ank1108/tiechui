use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Component, Path, PathBuf};

use tiechui_plugin_api::{CancellationToken, ToolError};

use super::{ContextFile, Diagnostic, FILENAMES, LoadedContext, MAX_BYTES, check};

pub(super) fn load(
    cwd: &Path,
    home: &Path,
    cancel: &CancellationToken,
) -> Result<(LoadedContext, Vec<Diagnostic>), ToolError> {
    let cwd = resolve(cwd)?;
    let home = resolve(home)?;
    let mut diagnostics = Vec::new();
    let mut paths = Vec::new();
    let mut seen = HashSet::new();
    if let Some(path) = candidate(&home, cancel, &mut diagnostics)? {
        seen.insert(path.clone());
        paths.push(path);
    }
    let shadowed = shadowed_file(&cwd, cancel, &mut diagnostics)?;
    let mut ancestors = Vec::new();
    for dir in cwd.ancestors() {
        if let Some(path) = candidate(dir, cancel, &mut diagnostics)? {
            let is_shadowed = shadowed.as_ref().is_some_and(|shadowed| {
                path.file_name()
                    .is_some_and(|name| canonical(dir).join(name) == *shadowed)
            });
            if !is_shadowed && seen.insert(path.clone()) {
                ancestors.push(path);
            }
        }
    }
    paths.extend(ancestors.into_iter().rev());
    let mut context = LoadedContext {
        files: Vec::new(),
        truncated: false,
    };
    let mut remaining = MAX_BYTES;
    for path in paths {
        check(cancel)?;
        if remaining == 0 {
            context.truncated = true;
            break;
        }
        match read(&path, remaining, cancel) {
            Ok((content, truncated)) => {
                remaining -= content.len();
                context.files.push(ContextFile { path, content });
                if truncated {
                    context.truncated = true;
                    break;
                }
            }
            Err(error) => {
                check(cancel)?;
                diagnostics.push(Diagnostic {
                    path,
                    message: error.to_string(),
                });
            }
        }
    }
    Ok((context, diagnostics))
}

// Discovery paths are resolved lexically; realpath is only for worktree identity.
fn resolve(path: &Path) -> Result<PathBuf, ToolError> {
    let mut resolved = PathBuf::new();
    for component in std::path::absolute(path)?.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            _ => resolved.push(component),
        }
    }
    Ok(resolved)
}

fn candidate(
    dir: &Path,
    cancel: &CancellationToken,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<PathBuf>, ToolError> {
    for name in FILENAMES {
        check(cancel)?;
        let path = dir.join(name);
        match fs::metadata(&path) {
            Ok(metadata) if metadata.is_file() => match File::open(&path) {
                Ok(_) => return Ok(Some(path)),
                Err(error) => diagnostics.push(Diagnostic {
                    path,
                    message: error.to_string(),
                }),
            },
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => diagnostics.push(Diagnostic {
                path,
                message: error.to_string(),
            }),
        }
    }
    Ok(None)
}

// Validate the entire file, including bytes beyond the retained prefix, with bounded memory.
fn read(path: &Path, limit: usize, cancel: &CancellationToken) -> io::Result<(String, bool)> {
    let mut file = File::open(path)?;
    let mut buffer = [0; 8192];
    let mut pending = Vec::new();
    let mut content = String::new();
    let mut length = 0;
    let mut first = true;
    loop {
        check(cancel).map_err(io::Error::other)?;
        let n = file.read(&mut buffer)?;
        if n == 0 {
            if !pending.is_empty() {
                return Err(io::Error::other("skipping non-UTF-8 context file"));
            }
            return Ok((content, length > limit));
        }
        pending.extend_from_slice(&buffer[..n]);
        let valid = match std::str::from_utf8(&pending) {
            Ok(text) => text,
            Err(error) if error.error_len().is_none() => {
                std::str::from_utf8(&pending[..error.valid_up_to()]).map_err(io::Error::other)?
            }
            Err(_) => return Err(io::Error::other("skipping non-UTF-8 context file")),
        };
        let consumed = valid.len();
        let text = if first {
            valid.strip_prefix('\u{feff}').unwrap_or(valid)
        } else {
            valid
        };
        if consumed > 0 {
            first = false;
        }
        let available = limit.saturating_sub(length);
        let mut end = text.len().min(available);
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        content.push_str(&text[..end]);
        length = length.saturating_add(text.len());
        pending.drain(..consumed);
    }
}

fn canonical(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_owned())
}

// A nested linked worktree shadows only the same-named file in its main worktree.
fn shadowed_file(
    cwd: &Path,
    cancel: &CancellationToken,
    diagnostics: &mut Vec<Diagnostic>,
) -> Result<Option<PathBuf>, ToolError> {
    for dir in cwd.ancestors() {
        check(cancel)?;
        let git = dir.join(".git");
        if git.is_dir() {
            return Ok(None);
        }
        if !git.exists() {
            continue;
        }
        let Some(target) = fs::read_to_string(&git)
            .ok()
            .and_then(|text| text.trim().strip_prefix("gitdir: ").map(str::to_owned))
        else {
            return Ok(None);
        };
        let git_dir = canonical(&dir.join(target.trim()));
        if !git_dir.join("HEAD").exists() {
            return Ok(None);
        }
        let common = fs::read_to_string(git_dir.join("commondir"))
            .map(|text| canonical(&git_dir.join(text.trim())))
            .unwrap_or(git_dir);
        let Some(main) = common.parent() else {
            return Ok(None);
        };
        let root = canonical(dir);
        if root != main && root.starts_with(main) && canonical(&main.join(".git")) == common {
            return Ok(candidate(dir, cancel, diagnostics)?
                .and_then(|path| path.file_name().map(|name| main.join(name))));
        }
        return Ok(None);
    }
    Ok(None)
}
