use std::path::{Component, Path, PathBuf};

use tiechui_plugin_api::ToolError;
use unicode_normalization::UnicodeNormalization;

use crate::policy::expand_home;

#[derive(Clone)]
pub(crate) struct Access {
    pub values: Vec<String>,
    pub lexical: PathBuf,
    pub canonical: PathBuf,
    pub direction: &'static str,
}

pub(crate) fn normalize(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for part in path.components() {
        match part {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            _ => result.push(part),
        }
    }
    result
}

pub(crate) fn canonical(path: &Path) -> Result<PathBuf, ToolError> {
    match path.canonicalize() {
        Ok(path) => Ok(path),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            let parent = path.parent().ok_or_else(|| ToolError(error.to_string()))?;
            let name = path
                .file_name()
                .ok_or_else(|| ToolError(error.to_string()))?;
            Ok(canonical(parent)?.join(name))
        }
        Err(error) => Err(ToolError(error.to_string())),
    }
}

impl Access {
    pub fn new(
        raw: &str,
        cwd: &Path,
        home: &Path,
        tool: Option<&str>,
        direction: &'static str,
    ) -> Result<Self, ToolError> {
        let mut spelling = raw.to_owned();
        if tool.is_some() {
            spelling = spelling
                .chars()
                .map(|c| match c {
                    '\u{a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => {
                        ' '
                    }
                    _ => c,
                })
                .collect();
            spelling = spelling.strip_prefix('@').unwrap_or(&spelling).into();
        }
        if spelling.starts_with("file://") {
            spelling = url::Url::parse(&spelling)
                .map_err(|e| ToolError(e.to_string()))?
                .to_file_path()
                .map_err(|_| ToolError("Invalid file URL path".into()))?
                .to_string_lossy()
                .into_owned();
        } else if tool.is_none() || spelling == "~" || spelling.starts_with("~/") {
            spelling = expand_home(&spelling, home);
        }
        let mut lexical = normalize(&cwd.join(&spelling));
        if tool == Some("read") && !lexical.exists() {
            let text = lexical.to_string_lossy();
            let variants = [
                text.replace(" AM.", "\u{202f}AM.")
                    .replace(" PM.", "\u{202f}PM."),
                text.nfd().collect(),
                text.replace('\'', "’"),
                text.nfd().collect::<String>().replace('\'', "’"),
            ];
            if let Some(path) = variants.into_iter().map(PathBuf::from).find(|p| p.exists()) {
                lexical = path;
            }
        }
        let canonical = canonical(&lexical)?;
        let mut values = vec![
            lexical.to_string_lossy().into_owned(),
            canonical.to_string_lossy().into_owned(),
        ];
        for path in [&lexical, &canonical] {
            if let Ok(relative) = path.strip_prefix(cwd) {
                values.push(relative.to_string_lossy().into_owned());
            }
        }
        // Preserve relative rules even when the spelling crosses the cwd boundary.
        if !Path::new(&spelling).is_absolute() {
            values.push(spelling);
        }
        values.dedup();
        Ok(Self {
            values,
            lexical,
            canonical,
            direction,
        })
    }

    /// Whether the access leaves the canonical `root` or the call's `cwd`,
    /// whose spelling may run through a symlink.
    pub fn outside(&self, root: &Path, cwd: &Path) -> bool {
        let real = canonical(cwd).unwrap_or_else(|_| cwd.to_owned());
        !(self.lexical.starts_with(cwd) || self.lexical.starts_with(&real))
            || !self.canonical.starts_with(&real)
            || !self.canonical.starts_with(root)
    }

    pub fn session_pattern(&self) -> String {
        self.lexical
            .parent()
            .unwrap_or(&self.lexical)
            .join("*")
            .to_string_lossy()
            .into_owned()
    }
}
