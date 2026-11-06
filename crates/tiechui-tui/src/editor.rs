//! A cancellable text editor backed by the main composer's editing model.
use crate::{composer::Composer, prompt::Outcome, theme::Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::text::{Line, Span};
use tiechui_protocol::ApprovalDecision;

pub struct Editor {
    title: String,
    composer: Composer,
    width: usize,
}
impl Editor {
    pub fn new(title: String, text: String) -> Self {
        let mut composer = Composer::default();
        composer.set_text(&text);
        Self {
            title,
            composer,
            width: 80,
        }
    }
    pub fn paste(&mut self, text: &str) {
        self.composer.insert(text);
    }
    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        match key.code {
            KeyCode::Esc => return Outcome::Answer(ApprovalDecision::Cancel),
            KeyCode::Char('c') if control => return Outcome::Answer(ApprovalDecision::Cancel),
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) || alt => {
                self.composer.newline()
            }
            KeyCode::Enter => {
                return Outcome::Answer(ApprovalDecision::Accept {
                    content: Some(serde_json::json!(self.composer.submission().0)),
                });
            }
            KeyCode::Char('a') if control => self.composer.home(),
            KeyCode::Char('e') if control => self.composer.end(),
            KeyCode::Char('u') if control => self.composer.delete_to_line_start(),
            KeyCode::Char('k') if control => self.composer.delete_to_line_end(),
            KeyCode::Char('w') if control => self.composer.delete_word_backward(),
            KeyCode::Char('z') if control => self.composer.undo(),
            KeyCode::Backspace if control || alt => self.composer.delete_word_backward(),
            KeyCode::Backspace => self.composer.backspace(),
            KeyCode::Delete => self.composer.delete_forward(),
            KeyCode::Left => self.composer.left(),
            KeyCode::Right => self.composer.right(),
            KeyCode::Home => self.composer.home(),
            KeyCode::End => self.composer.end(),
            KeyCode::Up => {
                self.composer.vertical(self.width.saturating_sub(2), true);
            }
            KeyCode::Down => {
                self.composer.vertical(self.width.saturating_sub(2), false);
            }
            KeyCode::Char(c) if !control && !alt => self.composer.insert(&c.to_string()),
            _ => {}
        }
        Outcome::Open
    }
    pub fn render(
        &mut self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        self.width = width;
        let rendered = self.composer.render(
            width,
            height.saturating_sub(2).max(1),
            Span::styled("› ", theme.accent),
            theme.dim,
        );
        let mut lines = vec![Line::from(Span::styled(self.title.clone(), theme.accent))];
        lines.extend(rendered.lines);
        lines.push(Line::from(Span::styled(
            "enter save · shift+enter newline · esc cancel",
            theme.dim,
        )));
        (lines, Some((rendered.cursor.0 + 1, rendered.cursor.1)))
    }
}
