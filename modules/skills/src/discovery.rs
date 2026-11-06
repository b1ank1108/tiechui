use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};

use ignore::gitignore::{Gitignore, GitignoreBuilder};
use tiechui_plugin_api::{CancellationToken, ToolError};

use crate::{Diagnostic, Root, SkillsModule, check, frontmatter};

pub(crate) fn load(
    roots: &[Root],
    result: &mut SkillsModule,
    cancel: &CancellationToken,
) -> Result<(), ToolError> {
    let mut files = HashSet::new();
    let mut collisions = Vec::new();
    for root in roots {
        check(cancel)?;
        let mut scan = Scan {
            root,
            cancel,
            result,
            files: &mut files,
            collisions: &mut collisions,
            active: HashSet::new(),
            rules: Vec::new(),
        };
        scan.directory(&root.path, true)?;
    }
    result.diagnostics.extend(collisions);
    Ok(())
}

struct Scan<'a> {
    root: &'a Root,
    cancel: &'a CancellationToken,
    result: &'a mut SkillsModule,
    files: &'a mut HashSet<PathBuf>,
    collisions: &'a mut Vec<Diagnostic>,
    active: HashSet<PathBuf>,
    rules: Vec<String>,
}

impl Scan<'_> {
    fn directory(&mut self, dir: &Path, root_files: bool) -> Result<(), ToolError> {
        check(self.cancel)?;
        let Ok(real) = dir.canonicalize() else {
            return Ok(());
        };
        if !self.active.insert(real.clone()) {
            self.result.diagnostics.push(Diagnostic::warning(
                dir,
                "skipping skill directory symlink cycle",
            ));
            return Ok(());
        }
        let result = self.entries(dir, root_files);
        self.active.remove(&real);
        result
    }

    fn entries(&mut self, dir: &Path, root_files: bool) -> Result<(), ToolError> {
        let Ok(entries) = fs::read_dir(dir) else {
            return Ok(());
        };
        let mut paths = Vec::new();
        for entry in entries {
            check(self.cancel)?;
            if let Ok(entry) = entry {
                paths.push(entry.path());
            }
        }
        paths.sort();
        let matcher = self.matcher(dir)?;
        let declared = dir.join("SKILL.md");
        if paths.contains(&declared) && declared.is_file() && !ignored(&matcher, &declared, false) {
            self.file(&declared)?;
            return Ok(());
        }
        for path in paths {
            check(self.cancel)?;
            let name = path.file_name().unwrap_or_default().to_string_lossy();
            if name.starts_with('.') || name == "node_modules" {
                continue;
            }
            let Ok(metadata) = path.metadata() else {
                continue;
            };
            if ignored(&matcher, &path, metadata.is_dir()) {
                continue;
            }
            if metadata.is_dir() {
                self.directory(&path, false)?;
            } else if metadata.is_file() && root_files && name.ends_with(".md") {
                self.file(&path)?;
            }
        }
        Ok(())
    }

    fn file(&mut self, path: &Path) -> Result<(), ToolError> {
        let Some(skill) =
            frontmatter::load(path, self.root, &mut self.result.diagnostics, self.cancel)?
        else {
            return Ok(());
        };
        let real = path.canonicalize().unwrap_or_else(|_| path.to_owned());
        if self.files.contains(&real) {
            return Ok(());
        }
        if let Some(winner) = self
            .result
            .skills
            .iter()
            .find(|other| other.name == skill.name)
        {
            self.collisions.push(Diagnostic {
                kind: "collision",
                path: path.to_owned(),
                message: format!("name \"{}\" collision", skill.name),
                winner_path: Some(winner.path.clone()),
            });
        } else {
            self.files.insert(real);
            self.result.skills.push(skill);
        }
        Ok(())
    }

    fn matcher(&mut self, dir: &Path) -> Result<Gitignore, ToolError> {
        let prefix = dir
            .strip_prefix(&self.root.path)
            .unwrap_or(dir)
            .to_string_lossy()
            .replace('\\', "/");
        let prefix = if prefix.is_empty() {
            prefix
        } else {
            format!("{prefix}/")
        };
        for filename in [".gitignore", ".ignore", ".fdignore"] {
            check(self.cancel)?;
            let Ok(content) = fs::read_to_string(dir.join(filename)) else {
                continue;
            };
            for line in content.lines() {
                check(self.cancel)?;
                if line.trim().is_empty() || line.trim().starts_with('#') {
                    continue;
                }
                let (negated, pattern) = if let Some(pattern) = line.strip_prefix('!') {
                    ("!", pattern)
                } else {
                    ("", line)
                };
                let pattern = if pattern.starts_with("\\!") {
                    &pattern[1..]
                } else {
                    pattern
                };
                let pattern = pattern.strip_prefix('/').unwrap_or(pattern);
                self.rules.push(format!("{negated}{prefix}{pattern}"));
            }
        }
        let mut builder = GitignoreBuilder::new(&self.root.path);
        for rule in &self.rules {
            check(self.cancel)?;
            let _ = builder.add_line(None, rule);
        }
        builder
            .build()
            .map_err(|error| ToolError(error.to_string()))
    }
}

fn ignored(matcher: &Gitignore, path: &Path, directory: bool) -> bool {
    matcher
        .matched_path_or_any_parents(path, directory)
        .is_ignore()
}
