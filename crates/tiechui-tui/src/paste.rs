//! Pasted text normalization and local image paths.

use std::path::{Path, PathBuf};

use base64::Engine;
use tokio::io::AsyncReadExt;
use tokio_util::sync::CancellationToken;

const IMAGE_LIMIT: u64 = 20 * 1024 * 1024;

/// LF line endings, expanded tabs, and no other control characters.
pub fn normalize(text: &str) -> String {
    text.replace("\r\n", "\n")
        .replace('\r', "\n")
        .replace('\t', "    ")
        .chars()
        .filter(|c| *c == '\n' || !c.is_control())
        .collect()
}

/// A single pasted path as terminals and file managers produce it: quoted, backslash-escaped,
/// `~/`-relative, or a `file://` URL.
fn path(text: &str, cwd: &Path) -> Option<PathBuf> {
    let text = text.trim();
    if text.is_empty() || text.contains('\n') {
        return None;
    }
    let unquoted = ['\'', '"']
        .iter()
        .find_map(|q| text.strip_prefix(*q)?.strip_suffix(*q))
        .map(str::to_owned);
    let raw = match unquoted {
        Some(raw) => raw,
        None if text.starts_with("file://") => {
            return url::Url::parse(text).ok()?.to_file_path().ok();
        }
        None if std::path::Path::new(text).is_absolute() => text.to_owned(),
        None => {
            let mut raw = String::new();
            let mut chars = text.chars();
            while let Some(c) = chars.next() {
                match c {
                    '\\' => raw.extend(chars.next()),
                    c if c.is_whitespace() => return None,
                    c => raw.push(c),
                }
            }
            raw
        }
    };
    if raw.starts_with("file://") {
        return url::Url::parse(&raw).ok()?.to_file_path().ok();
    }
    let path = match raw.strip_prefix("~/") {
        Some(rest) => tiechui_config::user_home()?.join(rest),
        None => cwd.join(raw),
    };
    Some(path)
}

fn mime_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

/// Reads a pasted image path into `(mime type, base64)`; anything else stays text.
pub fn image(text: &str, cwd: &Path) -> Option<(String, String)> {
    let path = path(text, cwd)?;
    let metadata = std::fs::metadata(&path).ok()?;
    if !metadata.is_file() || metadata.len() > IMAGE_LIMIT {
        return None;
    }
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .ok()?
        .take(IMAGE_LIMIT + 1)
        .read_to_end(&mut bytes)
        .ok()?;
    encoded_image(&bytes)
}

fn encoded_image(bytes: &[u8]) -> Option<(String, String)> {
    if bytes.len() as u64 > IMAGE_LIMIT {
        return None;
    }
    let mime_type = mime_type(bytes)?;
    Some((
        mime_type.to_owned(),
        base64::engine::general_purpose::STANDARD.encode(bytes),
    ))
}

#[derive(Debug, PartialEq, Eq)]
pub enum Clipboard {
    Image(String, String),
    Text(String),
}

async fn output(program: &str, args: &[&str], cancel: &CancellationToken) -> Option<Vec<u8>> {
    let mut child = tokio::process::Command::new(program)
        .args(args)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?.take(IMAGE_LIMIT * 2 + 100);
    let mut bytes = Vec::new();
    tokio::select! {
        biased;
        _ = cancel.cancelled() => None,
        result = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            stdout.read_to_end(&mut bytes).await.ok()?;
            if bytes.len() as u64 >= IMAGE_LIMIT * 2 + 100 { return None; }
            child.wait().await.ok()?.success().then_some(bytes)
        }) => result.ok().flatten(),
    }
}

fn clipboard_image(bytes: &[u8]) -> Option<Clipboard> {
    let image = encoded_image(bytes).or_else(|| {
        let text = std::str::from_utf8(bytes).ok()?.trim();
        let hex = text.strip_prefix("«data ")?.get(4..)?.strip_suffix('»')?;
        if hex.len() % 2 != 0 || !hex.is_ascii() {
            return None;
        }
        let bytes = (0..hex.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&hex[at..at + 2], 16).ok())
            .collect::<Option<Vec<_>>>()?;
        encoded_image(&bytes)
    })?;
    Some(Clipboard::Image(image.0, image.1))
}

/// Platform helpers only. Tests inject command output and never call the real clipboard.
pub async fn clipboard(cancel: CancellationToken) -> Option<Clipboard> {
    let mut commands: Vec<(&str, Vec<&str>)> = Vec::new();
    if cfg!(target_os = "macos") {
        commands.extend([
            ("osascript", vec!["-e", "get the clipboard as «class PNGf»"]),
            ("osascript", vec!["-e", "get the clipboard as «class JPEG»"]),
            ("osascript", vec!["-e", "get the clipboard as «class GIFf»"]),
            ("osascript", vec!["-e", "get the clipboard as «class WEBP»"]),
        ]);
    } else {
        for mime in ["image/png", "image/jpeg", "image/gif", "image/webp"] {
            commands.push(("wl-paste", vec!["--no-newline", "--type", mime]));
            commands.push(("xclip", vec!["-selection", "clipboard", "-o", "-t", mime]));
        }
    }
    for (program, args) in commands {
        if cancel.is_cancelled() {
            return None;
        }
        if let Some(bytes) = output(program, &args, &cancel).await
            && let Some(image) = clipboard_image(&bytes)
        {
            return Some(image);
        }
    }
    let commands = if cfg!(target_os = "macos") {
        vec![
            (
                "osascript",
                vec!["-e", "POSIX path of (the clipboard as alias)"],
            ),
            ("pbpaste", vec![]),
        ]
    } else {
        vec![
            ("wl-paste", vec!["--no-newline"]),
            ("xclip", vec!["-selection", "clipboard", "-o"]),
        ]
    };
    for (program, args) in commands {
        if cancel.is_cancelled() {
            return None;
        }
        if let Some(bytes) = output(program, &args, &cancel).await
            && let Ok(text) = String::from_utf8(bytes)
            && !text.trim().is_empty()
        {
            return Some(Clipboard::Text(text));
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalization_keeps_newlines_and_drops_controls() {
        assert_eq!(normalize("a\r\nb\rc\td\x1b[A"), "a\nb\nc    d[A");
    }

    #[tokio::test]
    async fn clipboard_helpers_validate_images_and_cancel_without_real_clipboard() {
        for (bytes, mime) in [
            (b"\x89PNG\r\n\x1a\n".as_slice(), "image/png"),
            (&[0xff, 0xd8, 0xff], "image/jpeg"),
            (b"GIF89a", "image/gif"),
            (b"RIFFabcdWEBP", "image/webp"),
        ] {
            let expected = Some(Clipboard::Image(
                mime.into(),
                base64::engine::general_purpose::STANDARD.encode(bytes),
            ));
            assert_eq!(clipboard_image(bytes), expected);
            let hex = bytes.iter().map(|b| format!("{b:02X}")).collect::<String>();
            assert_eq!(
                clipboard_image(format!("«data PNGf{hex}»\n").as_bytes()),
                expected
            );
        }
        assert!(clipboard_image(b"not an image").is_none());
        assert!(clipboard_image("«data PNGfZZ»".as_bytes()).is_none());
        assert!(encoded_image(&vec![0; IMAGE_LIMIT as usize + 1]).is_none());
        let cancel = CancellationToken::new();
        let (program, print_args, wait_args) = if cfg!(windows) {
            (
                "cmd.exe",
                &["/D", "/C", "<nul set /p =fixture&exit /b 0"][..],
                &["/D", "/C", "ping -n 11 127.0.0.1 >nul"][..],
            )
        } else {
            (
                "/bin/sh",
                &["-c", "printf fixture"][..],
                &["-c", "sleep 10"][..],
            )
        };
        assert_eq!(
            output(program, print_args, &cancel).await.unwrap(),
            b"fixture"
        );
        cancel.cancel();
        assert!(output(program, wait_args, &cancel).await.is_none());
    }

    #[test]
    fn image_paths_accept_quotes_escapes_urls_and_check_signatures() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("截图 1.png"), b"\x89PNG\r\n\x1a\nrest").unwrap();
        std::fs::write(dir.path().join("fake.png"), b"not an image").unwrap();
        for text in [
            "'截图 1.png'",
            "\"截图 1.png\"",
            "截图\\ 1.png",
            &format!(
                "{}",
                url::Url::from_file_path(dir.path().join("截图 1.png")).unwrap()
            ),
        ] {
            let (mime, data) = image(text, dir.path()).unwrap();
            assert_eq!(mime, "image/png", "{text}");
            assert!(data.starts_with("iVBORw0KGg"));
        }
        assert!(image("fake.png", dir.path()).is_none());
        assert!(image("截图 1.png", dir.path()).is_none());
        assert!(image("a.png\nb.png", dir.path()).is_none());
    }
}
