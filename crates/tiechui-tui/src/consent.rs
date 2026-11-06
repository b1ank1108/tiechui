//! Questions settled before a session opens, such as project trust.

use std::collections::BTreeSet;
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
    /// A text field. With choices it searches them; Enter answers the text when none match.
    pub input: Option<Input>,
    /// Space checks choices; Enter answers the checked ones, or the highlighted one alone.
    pub multiple: bool,
}

pub struct Input {
    pub text: String,
    /// Shown as bullets.
    pub secret: bool,
}

#[derive(Debug, PartialEq, Eq)]
pub enum Answer {
    Choice(usize),
    /// Checked choices, in order.
    Choices(Vec<usize>),
    Text(String),
    /// Esc.
    Dismissed,
    /// Ctrl+C or Ctrl+D.
    Interrupted,
}

/// Searched choices shown at once.
const ROWS: usize = 10;

struct Prompt<'a> {
    question: &'a Question,
    selected: usize,
    text: String,
    checked: BTreeSet<usize>,
}

impl<'a> Prompt<'a> {
    fn new(question: &'a Question) -> Self {
        Self {
            question,
            selected: 0,
            text: question
                .input
                .as_ref()
                .map(|input| input.text.clone())
                .unwrap_or_default(),
            checked: BTreeSet::new(),
        }
    }

    /// Indices of the choices the text matches, as a subsequence ignoring case.
    fn shown(&self) -> Vec<usize> {
        let query = self.text.to_lowercase();
        (0..self.question.choices.len())
            .filter(|index| {
                let choice = self.question.choices[*index].to_lowercase();
                let mut rest = choice.chars();
                query.chars().all(|c| rest.any(|n| n == c))
            })
            .collect()
    }

    /// Without a text field only Enter picks a choice: these answers grant trust, so digits
    /// just move the selection.
    fn key(&mut self, key: KeyEvent) -> Option<Answer> {
        if key.kind != KeyEventKind::Press {
            return None;
        }
        let typing = self.question.input.is_some();
        let shown = self.shown();
        let count = shown.len();
        let plain = !key
            .modifiers
            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
        match key.code {
            KeyCode::Char('c' | 'd') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return Some(Answer::Interrupted);
            }
            KeyCode::Esc => return Some(Answer::Dismissed),
            KeyCode::Enter => {
                let text = self.text.trim();
                if !self.checked.is_empty() {
                    return Some(Answer::Choices(self.checked.iter().copied().collect()));
                }
                return match shown.get(self.selected) {
                    Some(index) if self.question.multiple => Some(Answer::Choices(vec![*index])),
                    Some(index) => Some(Answer::Choice(*index)),
                    None if typing && !text.is_empty() => Some(Answer::Text(text.to_owned())),
                    None => None,
                };
            }
            KeyCode::Up if count > 0 => self.selected = (self.selected + count - 1) % count,
            KeyCode::Down if count > 0 => self.selected = (self.selected + 1) % count,
            KeyCode::Char(' ') if self.question.multiple => {
                if let Some(index) = shown.get(self.selected)
                    && !self.checked.remove(index)
                {
                    self.checked.insert(*index);
                }
            }
            KeyCode::Backspace if typing => {
                self.text.pop();
                self.selected = 0;
            }
            KeyCode::Char(c) if typing && plain => {
                self.text.push(c);
                self.selected = 0;
            }
            KeyCode::Char('k') if count > 0 => self.selected = (self.selected + count - 1) % count,
            KeyCode::Char('j') if count > 0 => self.selected = (self.selected + 1) % count,
            KeyCode::Char(digit @ '1'..='9') if digit as usize - ('1' as usize) < count => {
                self.selected = digit as usize - '1' as usize;
            }
            _ => {}
        }
        None
    }

    /// The lines, and the cursor's place in the text field.
    fn render(&self, width: usize, theme: &Theme) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
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
        let mut cursor = None;
        if let Some(input) = &question.input {
            let text = if input.secret {
                "•".repeat(self.text.chars().count())
            } else {
                clean(&self.text)
            };
            // The tail stays visible, where typing happens.
            let room = width.saturating_sub(5);
            let mut tail: String = text.chars().rev().collect::<String>();
            while wrap::width(&tail) > room {
                tail.pop();
            }
            let tail: String = tail.chars().rev().collect();
            cursor = Some((lines.len(), 4 + wrap::width(&tail)));
            lines.push(Line::from(vec![
                indent[0].clone(),
                Span::styled("> ", theme.dim),
                Span::raw(tail),
            ]));
            if !question.choices.is_empty() {
                lines.push(Line::default());
            }
        }
        let shown = self.shown();
        let rows = if question.input.is_some() {
            ROWS
        } else {
            shown.len()
        };
        let first = self
            .selected
            .saturating_sub(rows / 2)
            .min(shown.len().saturating_sub(rows));
        for (position, index) in shown.iter().enumerate().skip(first).take(rows) {
            let mark = match (question.multiple, self.checked.contains(index)) {
                (false, _) => "",
                (true, false) => "[ ] ",
                (true, true) => "[x] ",
            };
            lines.extend(choice(
                position,
                &format!("{mark}{}", clean(&question.choices[*index])),
                None,
                position == self.selected,
                width,
                theme,
            ));
        }
        if question.input.is_some() && !question.choices.is_empty() {
            let mut note = match shown.len() {
                0 => "No matches; enter uses the text".to_owned(),
                count if count > rows => format!("{}/{count}", self.selected + 1),
                _ => String::new(),
            };
            if !self.checked.is_empty() {
                let checked = format!("{} checked", self.checked.len());
                note = [note, checked]
                    .into_iter()
                    .filter(|part| !part.is_empty())
                    .collect::<Vec<_>>()
                    .join(" · ");
            }
            if !note.is_empty() {
                lines.push(Line::from(vec![
                    indent[0].clone(),
                    Span::styled(note, theme.dim),
                ]));
            }
        }
        lines.push(Line::default());
        let keys = match (
            &question.input,
            question.choices.is_empty(),
            question.multiple,
        ) {
            (None, _, _) => "↑↓ select · enter confirm",
            (Some(_), false, false) => "type to search · ↑↓ select · enter confirm",
            (Some(_), false, true) => "type to search · ↑↓ select · space check · enter confirm",
            (Some(_), true, _) => "enter confirm",
        };
        lines.push(wrap::fit(
            vec![
                indent[0].clone(),
                Span::styled(format!("{keys} · esc {}", question.dismiss), theme.dim),
            ],
            width,
            true,
        ));
        lines.push(Line::default());
        let lines = lines
            .into_iter()
            .map(|line| surface(line, width, theme))
            .collect();
        (lines, cursor)
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
    let mut prompt = Prompt::new(question);
    let mut screen = Screen::default();
    loop {
        let (columns, rows) = size()?;
        let (lines, cursor) = prompt.render(usize::from(columns), &theme);
        screen.frame(out, &[], &lines, cursor, usize::from(rows))?;
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
            input: None,
            multiple: false,
        }
    }

    fn press(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    #[test]
    fn only_enter_picks_and_digits_move_the_selection() {
        let question = question();
        let mut prompt = Prompt::new(&question);
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
        let mut prompt = Prompt::new(&question);
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
        let mut prompt = Prompt::new(&question);
        prompt.selected = 1;
        let theme = Theme::fixture(ColorLevel::TrueColor, false);
        let text: Vec<String> = prompt
            .render(40, &theme)
            .0
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

    #[test]
    fn typing_searches_the_choices_or_answers_the_text() {
        let mut question = question();
        question.input = Some(Input {
            text: String::new(),
            secret: false,
        });
        let mut prompt = Prompt::new(&question);
        for c in "kp".chars() {
            assert_eq!(prompt.key(press(KeyCode::Char(c))), None);
        }
        assert_eq!(prompt.shown(), [1]);
        assert_eq!(prompt.key(press(KeyCode::Enter)), Some(Answer::Choice(1)));
        prompt.key(press(KeyCode::Char('x')));
        assert_eq!(prompt.shown(), Vec::<usize>::new());
        assert_eq!(
            prompt.key(press(KeyCode::Enter)),
            Some(Answer::Text("kpx".into()))
        );
    }

    #[test]
    fn space_checks_choices_while_the_search_changes() {
        let mut question = question();
        question.input = Some(Input {
            text: String::new(),
            secret: false,
        });
        question.multiple = true;
        let mut prompt = Prompt::new(&question);
        prompt.key(press(KeyCode::Char('q')));
        prompt.key(press(KeyCode::Char(' ')));
        prompt.key(press(KeyCode::Backspace));
        prompt.key(press(KeyCode::Char(' ')));
        let text: Vec<String> = prompt
            .render(40, &Theme::fixture(ColorLevel::TrueColor, false))
            .0
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .map(|line| line.trim_end().to_owned())
            .collect();
        for row in ["  › 1. [x] Trust", "    3. [x] Quit", "  2 checked"] {
            assert!(text.iter().any(|line| line == row), "{text:?}");
        }
        assert_eq!(
            prompt.key(press(KeyCode::Enter)),
            Some(Answer::Choices(vec![0, 2]))
        );
        assert_eq!(
            Prompt::new(&question).key(press(KeyCode::Enter)),
            Some(Answer::Choices(vec![0]))
        );
    }

    #[test]
    fn a_secret_field_shows_bullets_with_the_cursor_after_them() {
        let question = Question {
            title: "Key".into(),
            body: vec![],
            choices: vec![],
            dismiss: "back",
            input: Some(Input {
                text: "sk-1".into(),
                secret: true,
            }),
            multiple: false,
        };
        let prompt = Prompt::new(&question);
        let (lines, cursor) = prompt.render(40, &Theme::fixture(ColorLevel::TrueColor, false));
        let (row, column) = cursor.unwrap();
        let text: String = lines[row]
            .spans
            .iter()
            .map(|s| s.content.as_ref())
            .collect();
        assert_eq!(text.trim_end(), "  > ••••");
        assert_eq!(column, 8);
        assert_eq!(
            Prompt::new(&question).key(press(KeyCode::Enter)),
            Some(Answer::Text("sk-1".into()))
        );
    }
}
