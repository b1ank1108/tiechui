//! Terminal setup and the OSC 10/11 color query.

use crate::palette::Rgb;

pub type Colors = (Option<Rgb>, Option<Rgb>);

/// Crossterm exposes OSC as Alt+] followed by keys. Keep those replies out of editors,
/// including replies fragmented across event-loop iterations or arriving after startup.
#[derive(Default)]
pub struct ReplyFilter {
    pending: Vec<crossterm::event::KeyEvent>,
    text: String,
    escape: bool,
}

impl ReplyFilter {
    pub fn key(
        &mut self,
        key: crossterm::event::KeyEvent,
    ) -> (Vec<crossterm::event::KeyEvent>, Option<Colors>) {
        use crossterm::event::{KeyCode, KeyModifiers};
        if self.pending.is_empty() {
            if key.code == KeyCode::Char(']') && key.modifiers == KeyModifiers::ALT {
                self.pending.push(key);
                self.text = "\x1b]".into();
                return (vec![], None);
            }
            return (vec![key], None);
        }
        if self.pending.len() < 128 {
            self.pending.push(key);
        }
        if key.code == KeyCode::Esc {
            self.escape = true;
            return (vec![], None);
        }
        let end = (key.code == KeyCode::Char('g') && key.modifiers == KeyModifiers::CONTROL)
            || (key.code == KeyCode::Char('\\')
                && (key.modifiers == KeyModifiers::ALT || self.escape));
        self.escape = false;
        if end {
            self.text.push('\x07');
            let colors = parse_replies(self.text.as_bytes());
            self.pending.clear();
            self.text.clear();
            return (vec![], Some(colors));
        }
        if let KeyCode::Char(ch) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::ALT | KeyModifiers::CONTROL | KeyModifiers::SUPER)
        {
            if self.text.len() < 128 {
                self.text.push(ch);
            }
            let body = &self.text[2..];
            if ["10;", "11;"]
                .iter()
                .any(|prefix| prefix.starts_with(body) || body.starts_with(prefix))
            {
                return (vec![], None);
            }
        }
        self.text.clear();
        (std::mem::take(&mut self.pending), None)
    }
}

pub fn reflow_cap(env: &dyn Fn(&str) -> Option<String>) -> usize {
    let program = env("TERM_PROGRAM").unwrap_or_default().to_lowercase();
    if program == "vscode" || env("VSCODE_INJECTION").is_some() {
        return 1_000;
    }
    if env("WT_SESSION").is_some() {
        return 9_001;
    }
    if program == "wezterm" || env("WEZTERM_PANE").is_some() {
        return 3_500;
    }
    if program == "alacritty" || env("TERM").as_deref() == Some("alacritty") {
        return 10_000;
    }
    1_000
}

/// Parses `rgb:R/G/B` with 1–4 hex digits per channel, as xterm reports.
fn parse_rgb(value: &str) -> Option<Rgb> {
    let channels: Vec<u8> = value
        .strip_prefix("rgb:")?
        .split('/')
        .map(|part| {
            let digits = u32::try_from(part.len())
                .ok()
                .filter(|d| (1..=4).contains(d))?;
            let raw = u32::from_str_radix(part, 16).ok()?;
            let max = (1u32 << (4 * digits)) - 1;
            Some(((raw * 255 + max / 2) / max) as u8)
        })
        .collect::<Option<_>>()?;
    match channels[..] {
        [r, g, b] => Some(Rgb(r, g, b)),
        _ => None,
    }
}

/// Extracts OSC 10 (foreground) and 11 (background) replies terminated by BEL or ST.
pub fn parse_replies(bytes: &[u8]) -> (Option<Rgb>, Option<Rgb>) {
    let text = String::from_utf8_lossy(bytes);
    let mut foreground = None;
    let mut background = None;
    for reply in text.split("\x1b]").skip(1) {
        let end = reply.find(['\x07', '\x1b']).unwrap_or(reply.len());
        let Some((code, value)) = reply[..end].split_once(';') else {
            continue;
        };
        match code {
            "10" => foreground = parse_rgb(value),
            "11" => background = parse_rgb(value),
            _ => {}
        }
    }
    (foreground, background)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_with_bel_st_and_short_channels_parse() {
        let bytes = b"\x1b]10;rgb:d0d0/d0d0/d0d0\x07\x1b]11;rgb:1e/1e/1e\x1b\\\x1b[?62;22c";
        assert_eq!(
            parse_replies(bytes),
            (Some(Rgb(0xd0, 0xd0, 0xd0)), Some(Rgb(0x1e, 0x1e, 0x1e)))
        );
        assert_eq!(parse_rgb("rgb:f/8/0"), Some(Rgb(255, 136, 0)));
    }

    #[test]
    fn missing_or_malformed_replies_stay_unknown() {
        assert_eq!(parse_replies(b"\x1b[?62c"), (None, None));
        assert_eq!(parse_replies(b"\x1b]11;rgb:zz/00/00\x07"), (None, None));
    }
}
