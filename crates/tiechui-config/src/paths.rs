use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::ConfigError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigPaths {
    pub user: PathBuf,
    pub project: Option<PathBuf>,
}

impl ConfigPaths {
    pub fn providers(&self) -> PathBuf {
        self.user.with_file_name("providers.toml")
    }

    /// Find the closest project config without crossing the nearest Git root.
    /// Outside a Git repository, only the supplied directory is considered.
    pub fn discover(cwd: &Path) -> Result<Self, ConfigError> {
        Self::discover_with_env(cwd, |name| std::env::var_os(name))
    }

    /// Discover user and project config paths using an explicit profile directory.
    pub fn for_home(home: &Path, cwd: &Path) -> Result<Self, ConfigError> {
        Self::discover_with_env(cwd, |name| {
            (name == "TIECHUI_HOME").then(|| home.as_os_str().to_owned())
        })
    }

    /// Discover the Tiechui profile directory (`$TIECHUI_HOME` or `~/.tiechui`).
    pub fn discover_home() -> Result<PathBuf, ConfigError> {
        let home = Self::resolve_home_with_env(|name| std::env::var_os(name))?;
        if home.is_absolute() {
            Ok(home)
        } else {
            let cwd = std::env::current_dir()
                .map_err(|error| ConfigError::new(".", "<directory>", error.to_string()))?;
            Ok(cwd.join(home))
        }
    }

    fn resolve_home_with_env(
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<PathBuf, ConfigError> {
        match env("TIECHUI_HOME") {
            Some(value) if !value.is_empty() => Ok(PathBuf::from(value)),
            Some(_) => Err(ConfigError::new(
                "<environment>",
                "TIECHUI_HOME",
                "must not be empty",
            )),
            None => {
                let home = env("HOME")
                    .or_else(|| env("USERPROFILE"))
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        ConfigError::new(
                            "<environment>",
                            "HOME",
                            "set TIECHUI_HOME or a home directory",
                        )
                    })?;
                Ok(PathBuf::from(home).join(".tiechui"))
            }
        }
    }

    pub(crate) fn discover_with_env(
        cwd: &Path,
        env: impl Fn(&str) -> Option<OsString>,
    ) -> Result<Self, ConfigError> {
        let cwd = cwd
            .canonicalize()
            .map_err(|error| ConfigError::new(cwd, "<directory>", error.to_string()))?;
        if !cwd.is_dir() {
            return Err(ConfigError::new(
                &cwd,
                "<directory>",
                "expected a directory",
            ));
        }
        let home = Self::resolve_home_with_env(env)?;
        let user = if home.is_absolute() {
            home
        } else {
            cwd.join(home)
        }
        .join("config.toml");
        let canonical_user = user.canonicalize().ok();
        let mut root = cwd.as_path();
        for ancestor in cwd.ancestors() {
            if ancestor
                .join(".git")
                .try_exists()
                .map_err(|error| ConfigError::new(ancestor, ".git", error.to_string()))?
            {
                root = ancestor;
                break;
            }
        }
        let mut project = None;
        'ancestors: for ancestor in cwd.ancestors() {
            for name in [".agents/config.toml", ".claude/config.toml"] {
                let path = ancestor.join(name);
                if path
                    .try_exists()
                    .map_err(|error| ConfigError::new(&path, "<file>", error.to_string()))?
                {
                    let canonical = path
                        .canonicalize()
                        .map_err(|error| ConfigError::new(&path, "<file>", error.to_string()))?;
                    if canonical_user.as_ref() != Some(&canonical) {
                        project = Some(path);
                        break 'ancestors;
                    }
                }
            }
            if ancestor == root {
                break;
            }
        }
        Ok(Self { user, project })
    }
}
