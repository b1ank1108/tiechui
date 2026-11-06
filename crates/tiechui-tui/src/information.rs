use crossterm::event::{KeyCode, KeyEvent};
use ratatui_core::text::{Line, Span};
use tiechui_protocol::ApprovalDecision;

use crate::{prompt::Outcome, theme::Theme, wrap};

pub struct Information {
    title: String,
    text: String,
    scroll: usize,
}

impl Information {
    pub fn new(title: String, text: String) -> Self {
        Self {
            title,
            text,
            scroll: 0,
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        match key.code {
            KeyCode::Esc | KeyCode::Enter | KeyCode::Char('q') => {
                return Outcome::Answer(ApprovalDecision::Cancel);
            }
            KeyCode::Up | KeyCode::Char('k') => self.scroll = self.scroll.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.scroll += 1,
            KeyCode::PageUp => self.scroll = self.scroll.saturating_sub(8),
            KeyCode::PageDown => self.scroll += 8,
            KeyCode::Home => self.scroll = 0,
            KeyCode::End => self.scroll = usize::MAX,
            _ => {}
        }
        Outcome::Open
    }

    pub fn render(&mut self, width: usize, height: usize, theme: &Theme) -> Vec<Line<'static>> {
        let body = self
            .text
            .split('\n')
            .flat_map(|line| wrap::wrap(&[Span::raw(line.to_owned())], width, &[], &[]))
            .collect::<Vec<_>>();
        let room = height.saturating_sub(2).max(1);
        self.scroll = self.scroll.min(body.len().saturating_sub(room));
        let mut lines = vec![wrap::fit(
            vec![Span::styled(self.title.clone(), theme.accent)],
            width,
            false,
        )];
        lines.extend(body.into_iter().skip(self.scroll).take(room));
        lines.push(wrap::fit(
            vec![Span::styled("↑/↓ scroll · enter/esc close", theme.dim)],
            width,
            false,
        ));
        lines.truncate(height);
        lines
    }
}
