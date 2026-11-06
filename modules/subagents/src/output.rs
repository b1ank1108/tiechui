use std::{
    fs::File,
    io::Write,
    path::{Path, PathBuf},
};
use tiechui_plugin_api::ToolError;
use unicode_segmentation::UnicodeSegmentation;

pub const PREVIEW_BYTES: usize = 50 * 1024;
const ARCHIVE_BYTES: usize = 64 * 1024 * 1024;

pub fn prefix(text: &str, bytes: usize) -> String {
    let mut length = 0;
    text.graphemes(true)
        .take_while(|g| {
            length += g.len();
            length <= bytes
        })
        .collect()
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt, process::Command};

    #[test]
    fn spill_directories_are_private_with_permissive_umask() {
        if std::env::var_os("TIECHUI_SPILL_UMASK_CHILD").is_none() {
            let result = Command::new("/bin/sh")
                .args(["-c", "umask 000; exec \"$@\"", "test"])
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "output::tests::spill_directories_are_private_with_permissive_umask",
                    "--nocapture",
                ])
                .env("TIECHUI_SPILL_UMASK_CHILD", "1")
                .output()
                .unwrap();
            assert!(result.status.success(), "{result:?}");
            return;
        }
        let root = tempfile::tempdir_in(env!("CARGO_MANIFEST_DIR")).unwrap();
        let directory = root.path().join("profile/state/subagents/output");
        let mut output = Output::new(&directory);
        output.push(&"x".repeat(PREVIEW_BYTES + 1)).unwrap();
        for path in directory.ancestors().take(4) {
            assert_eq!(
                fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700,
                "{}",
                path.display()
            );
        }
        assert_eq!(
            fs::metadata(output.path.unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

pub struct Output {
    pub text: String,
    pub path: Option<PathBuf>,
    file: Option<File>,
    bytes: usize,
    directory: PathBuf,
}
impl Output {
    pub fn new(directory: &Path) -> Self {
        Self {
            text: String::new(),
            path: None,
            file: None,
            bytes: 0,
            directory: directory.into(),
        }
    }
    pub fn clear(&mut self) {
        self.text.clear();
        self.path = None;
        self.file = None;
        self.bytes = 0;
    }
    pub fn push(&mut self, text: &str) -> Result<(), ToolError> {
        if self.bytes.saturating_add(text.len()) > ARCHIVE_BYTES {
            return Err(ToolError("Child output archive exceeded 64 MiB".into()));
        }
        if self.file.is_none() && self.bytes + text.len() > PREVIEW_BYTES {
            tiechui_plugin_api::private::create_dir_all(&self.directory)?;
            let file = tempfile::NamedTempFile::new_in(&self.directory)?;
            let (mut file, path) = file.keep().map_err(|e| ToolError(e.to_string()))?;
            file.write_all(self.text.as_bytes())?;
            self.file = Some(file);
            self.path = Some(path);
        }
        if let Some(file) = &mut self.file {
            file.write_all(text.as_bytes())?;
        }
        if self.text.len() < PREVIEW_BYTES {
            self.text
                .push_str(&prefix(text, PREVIEW_BYTES - self.text.len()));
        }
        self.bytes += text.len();
        Ok(())
    }
    pub fn full(&self) -> Result<String, ToolError> {
        match &self.path {
            Some(path) => Ok(std::fs::read_to_string(path)?),
            None => Ok(self.text.clone()),
        }
    }
}
