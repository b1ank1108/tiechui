//! OSC 52 clipboard requests, with a UTF-8 payload budget and tmux passthrough.

use base64::{Engine, engine::general_purpose::STANDARD};

/// Returns the terminal bytes, or `None` when the text exceeds 100,000 UTF-8 bytes.
pub fn encode(text: &str, tmux: bool) -> Option<String> {
    if text.len() > 100_000 {
        return None;
    }
    let plain = format!("\x1b]52;c;{}\x1b\\", STANDARD.encode(text));
    if tmux {
        Some(format!(
            "{plain}\x1bPtmux;{}\x1b\\",
            plain.replace('\x1b', "\x1b\x1b")
        ))
    } else {
        Some(plain)
    }
}
