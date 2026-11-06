//! Questions settled before a session opens, such as project trust.

use std::io::{self, Write};
use std::time::Duration;

use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::terminal::{enable_raw_mode, size};
use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::palette::{Palette, detect_color_level};
use crate::prompt::choice;
use crate::render::Screen;
use crate::rewind::clean;
use crate::status_picker::surface;
use crate::theme::Theme;
use crate::wrap;

pub struct Question {
    pub title: String,
    /// One wrapped paragraph each; an empty one is a blank row. May quote untrusted files.
    pub body: Vec<String>,
    pub choices: Vec<String>,
    /// What Esc does, for the key hint.
    pub dismiss: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Choice(usize),
    /// Esc.
    Dismissed,
    /// Ctrl+C or Ctrl+D.
    Interrupted,
}

struct Prompt<'a> {
    question: &'a Question,
    selected: usize,
}

impl Prompt<'_> {
    /// Only Enter picks a choice: these answers grant trust, so digits just move the selection.
    fn key(&mut self, key: KeyEvent) -> Option<Answer> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let count = self.question.choices.len();
        match key.code {
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(Answer::Interrupted);
            }
            KeyCode::Esc => return Some(Answer::Dismissed),
            KeyCode::Enter => return Some(Answer::Choice(self.selected)),
            KeyCode::Up | KeyCode::Char('k') => self.selected = (self.selected + count - 1) % count,
            KeyCode::Down | KeyCode::Char('j') => self.selected = (self.selected + 1) % count,
            KeyCode::Char(digit @ '1'..='9') if digit as usize - ('1' as usize) < count => {
                self.selected = digit as usize - '1' as usize;
            }
            _ => {}
        }
        None
    }

    fn render(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let question = self.question;
        let indent = [Span::raw("  ")];
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let mut lines = vec![Line::default()];
        lines.extend(wrap::wrap(
            &[Span::styled(clean(&question.title), bold)],
            width,
            &indent,
            &indent,
        ));
        for paragraph in &question.body {
            if paragraph.is_empty() {
                lines.push(Line::default());
            } else {
                lines.extend(wrap::wrap(
                    &[Span::raw(clean(paragraph))],
                    width,
                    &indent,
                    &indent,
                ));
            }
        }
        lines.push(Line::default());
        for (index, label) in question.choices.iter().enumerate() {
            lines.extend(choice(
                index,
                &clean(label),
                None,
                index == self.selected,
                width,
                theme,
            ));
        }
        lines.push(Line::default());
        lines.push(wrap::fit(
            vec![
                indent[0].clone(),
                Span::styled(
                    format!("↑↓ select · enter confirm · esc {}", question.dismiss),
                    theme.dim,
                ),
            ],
            width,
            true,
        ));
        lines.push(Line::default());
        lines
            .into_iter()
            .map(|line| surface(line, width, theme))
            .collect()
    }
}

/// Asks on the terminal, before the interactive UI takes it over.
pub fn ask(question: &Question) -> io::Result<Answer> {
    enable_raw_mode()?;
    let result = run(question, &mut io::stdout());
    crate::restore();
    result
}

fn run(question: &Question, out: &mut impl Write) -> io::Result<Answer> {
    // Keys typed before the question was visible must not answer it.
    while event::poll(Duration::ZERO)? {
        event::read()?;
    }
    let env = |key: &str| std::env::var(key).ok();
    let theme = Theme::new(Palette {
        level: detect_color_level(&env),
        foreground: None,
        background: None,
    });
    let mut prompt = Prompt {
        question,
        selected: 0,
    };
    let mut screen = Screen::default();
    loop {
        let (columns, rows) = size()?;
        let lines = prompt.render(usize::from(columns), &theme);
        screen.frame(out, &[], &lines, None, usize::from(rows))?;
        if let Event::Key(key) = event::read()?
            && let Some(answer) = prompt.key(key)
        {
            screen.clear(out)?;
            return Ok(answer);
        }
    }
}

#[cfg(test)]
mod tests {
    use crossterm::event::KeyEventState;

    use super::*;
    use crate::palette::ColorLevel;

    fn question() -> Question {
        Question {
            title: "Trust this folder?".into(),
            body: vec!["/work\x1b[2K/project".into(), String::new(), "text".into()],
            choices: vec!["Trust".into(), "Skip".into(), "Quit".into()],
            dismiss: "quit",
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn only_enter_picks_and_digits_move_the_selection() {
        let question = question();
        let mut prompt = Prompt {
            question: &question,
            selected: 0,
        };
        assert_eq!(prompt.key(press(KeyCode::Char('2'))), None);
        assert_eq!(prompt.key(press(KeyCode::Char('9'))), None);
        assert_eq!(prompt.selected, 1);
        assert_eq!(prompt.key(press(KeyCode::Down)), None);
        assert_eq!(prompt.key(press(KeyCode::Down)), None);
        assert_eq!(prompt.key(press(KeyCode::Up)), None);
        assert_eq!(prompt.selected, 2);
        assert_eq!(
            prompt.key(KeyEvent {
                kind: KeyEventKind::Release,
                state: KeyEventState::NONE,
                ..press(KeyCode::Enter)
            }),
            None
        );
        assert_eq!(prompt.key(press(KeyCode::Enter)), Some(Answer::Choice(2)));
    }

    #[test]
    fn esc_dismisses_and_control_keys_interrupt() {
        let question = question();
        let mut prompt = Prompt {
            question: &question,
            selected: 0,
        };
        assert_eq!(prompt.key(press(KeyCode::Esc)), Some(Answer::Dismissed));
        for letter in ['c', 'd'] {
            assert_eq!(
                prompt.key(KeyEvent::new(KeyCode::Char(letter), KeyModifiers::CONTROL)),
                Some(Answer::Interrupted)
            );
        }
    }

    #[test]
    fn quoted_file_contents_cannot_reach_the_terminal() {
        let question = question();
        let prompt = Prompt {
            question: &question,
            selected: 1,
        };
        let theme = Theme::fixture(ColorLevel::TrueColor, false);
        let text: Vec<String> = prompt
            .render(40, &theme)
            .iter()
            .map(|line| {
                let text: String = line.spans.iter().map(|s| s.content.as_ref()).collect();
                text.trim_end().to_owned()
            })
            .collect();
        assert!(text.iter().all(|line| !line.contains('\x1b')), "{text:?}");
        for row in [
            "  /work [2K/project",
            "  › 2. Skip",
            "  ↑↓ select · enter confirm · esc quit",
        ] {
            assert!(text.iter().any(|line| line == row), "{text:?}");
        }
    }
}
