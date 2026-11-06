//! The transcript pager: the whole conversation expanded on the alternate screen.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::text::{Line, Span};

use crate::theme::Theme;
use crate::wrap;

#[derive(Debug)]
pub struct Pager {
    offset: usize,
    following: bool,
    height: usize,
    count: usize,
}

impl Default for Pager {
    fn default() -> Self {
        Self {
            offset: 0,
            following: true,
            height: 1,
            count: 0,
        }
    }
}

impl Pager {
    /// A fixed snapshot starts at its first line rather than following a transcript.
    pub fn from_top() -> Self {
        Self {
            following: false,
            ..Self::default()
        }
    }

    /// Shared viewport for alternate-screen inspectors with their own chrome.
    pub fn viewport(&mut self, lines: &[Line<'static>], height: usize) -> Vec<Line<'static>> {
        self.viewport_parts(lines, &[], height)
    }

    pub fn following(&self) -> bool {
        self.following
    }

    pub fn follow(&mut self) {
        self.following = true;
    }

    /// The line drawn at `row` of the last viewport.
    pub fn line(&self, row: usize) -> Option<usize> {
        (row < self.height && self.offset + row < self.count).then_some(self.offset + row)
    }

    /// Scrolls back to `line` when a reader has moved past it; following stays at the bottom.
    pub fn reveal(&mut self, line: usize) {
        if !self.following {
            self.offset = self.offset.min(line);
        }
    }

    pub fn viewport_parts(
        &mut self,
        lines: &[Line<'static>],
        tail: &[Line<'static>],
        height: usize,
    ) -> Vec<Line<'static>> {
        self.height = height.max(1);
        self.count = lines.len() + tail.len();
        if self.following {
            self.offset = self.max();
        }
        self.offset = self.offset.min(self.max());
        lines
            .iter()
            .chain(tail)
            .skip(self.offset)
            .take(self.height)
            .cloned()
            .collect()
    }

    pub fn position(&self) -> (usize, usize, usize) {
        (
            self.offset + 1,
            (self.offset + self.height).min(self.count),
            self.count,
        )
    }

    fn max(&self) -> usize {
        self.count.saturating_sub(self.height)
    }

    pub fn scroll(&mut self, delta: isize) {
        self.offset = self.offset.saturating_add_signed(delta).min(self.max());
        self.following = self.offset >= self.max();
    }

    /// Returns false when the pager closes.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let page = isize::try_from(self.height).unwrap_or(isize::MAX);
        let half = isize::try_from(self.height.div_ceil(2)).unwrap_or(isize::MAX);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return false,
            KeyCode::Char('c') if control => return false,
            KeyCode::Up | KeyCode::Char('k') => self.scroll(-1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll(1),
            KeyCode::Char('b') if control => self.scroll(-page),
            KeyCode::Char('f') if control => self.scroll(page),
            KeyCode::Char('u') if control => self.scroll(-half),
            KeyCode::Char('d') if control => self.scroll(half),
            KeyCode::PageUp => self.scroll(-page),
            KeyCode::Char(' ') if shift => self.scroll(-page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.scroll(page),
            KeyCode::Home => self.scroll(isize::MIN),
            KeyCode::End => self.scroll(isize::MAX),
            _ => {}
        }
        true
    }

    /// Exactly `rows` lines: header, body, a position rule and the key hints.
    pub fn render(
        &mut self,
        lines: &[Line<'static>],
        width: usize,
        rows: usize,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        let rows = rows.max(1);
        let chrome = if rows >= 6 {
            5
        } else if rows >= 3 {
            2
        } else {
            0
        };
        self.height = (rows - chrome).max(1);
        self.count = lines.len();
        if self.following {
            self.offset = self.max();
        }
        self.offset = self.offset.min(self.max());
        self.following = self.offset >= self.max();
        let mut body: Vec<Line<'static>> = lines
            .iter()
            .skip(self.offset)
            .take(self.height)
            .cloned()
            .collect();
        body.resize(self.height, Line::default());
        if chrome == 0 {
            return body;
        }
        let dim = |text: String| wrap::fit(vec![Span::styled(text, theme.dim)], width, false);
        let title = "/ T R A N S C R I P T ";
        let header = dim(format!("{title}{}", "/ ".repeat(width.div_ceil(2))));
        let close = dim("q / ctrl+c to quit · esc back".into());
        let mut screen = vec![header];
        screen.extend(body);
        if chrome == 2 {
            screen.push(close);
            return screen;
        }
        let percent = if self.max() == 0 {
            100
        } else {
            self.offset * 100 / self.max()
        };
        let label = format!(" {percent}% ");
        let rule = width.saturating_sub(wrap::width(&label) + 1);
        screen.push(dim(format!("{}{label}─", "─".repeat(rule))));
        screen.push(dim(
            "↑/↓ to scroll · pgup/pgdn to page · home/end to jump".into()
        ));
        screen.push(close);
        screen.push(Line::default());
        screen
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::ColorLevel;

    fn text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn follows_the_end_until_scrolled_and_pages_by_visible_height() {
        let theme = Theme::fixture(ColorLevel::TrueColor, false);
        let content: Vec<Line<'static>> = (1..=20).map(|n| Line::from(format!("{n}"))).collect();
        let mut pager = Pager::default();
        let screen = text(&pager.render(&content, 30, 10, &theme));
        assert_eq!(screen.len(), 10);
        assert_eq!(screen[1], "16");
        assert_eq!(screen[5], "20");
        assert!(screen[6].contains(" 100% "));
        pager.key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE));
        pager.key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        let screen = text(&pager.render(&content, 30, 10, &theme));
        assert_eq!(screen[1], "12");
        pager.key(KeyEvent::new(KeyCode::Home, KeyModifiers::NONE));
        assert_eq!(text(&pager.render(&content, 30, 10, &theme))[1], "1");
        pager.key(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        assert_eq!(text(&pager.render(&content, 30, 10, &theme))[1], "6");
        assert!(!pager.key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE)));
    }
}
