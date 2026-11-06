//! Question drafts are local until an explicit submission.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use serde_json::{Value, json};
use tiechui_protocol::{ApprovalDecision, Question};

use crate::{composer::Composer, prompt::Outcome, status_picker::surface, theme::Theme, wrap};

const OTHER: &str = "None of the above";

#[derive(Default)]
struct Draft {
    selected: Option<usize>,
    committed: bool,
    notes: bool,
    editor: Composer,
}

pub struct Ask {
    questions: Vec<Question>,
    drafts: Vec<Draft>,
    current: usize,
    confirmation: Option<usize>,
    option_offset: usize,
    text_focus: bool,
}

impl Ask {
    pub fn new(questions: Vec<Question>) -> Self {
        let text_focus = questions
            .first()
            .is_some_and(|question| question.options.is_none());
        let drafts = questions
            .iter()
            .map(|question| Draft {
                selected: question.options.as_ref().map(|_| 0),
                notes: question.options.is_none(),
                ..Draft::default()
            })
            .collect();
        Self {
            questions,
            drafts,
            current: 0,
            confirmation: None,
            option_offset: 0,
            text_focus,
        }
    }

    fn answered(&self, index: usize) -> bool {
        let draft = &self.drafts[index];
        draft.committed
            && if self.questions[index].options.is_some() {
                draft.selected.is_some()
            } else {
                !draft.editor.submission().0.trim().is_empty()
            }
    }

    fn unanswered(&self) -> usize {
        (0..self.questions.len())
            .filter(|&i| !self.answered(i))
            .count()
    }

    fn response(&self) -> Outcome {
        let answers: serde_json::Map<String, Value> = self
            .questions
            .iter()
            .enumerate()
            .map(|(i, question)| {
                let mut answers = vec![];
                if self.answered(i) {
                    let draft = &self.drafts[i];
                    if let (Some(options), Some(selected)) = (&question.options, draft.selected) {
                        answers.push(
                            options
                                .get(selected)
                                .map_or(OTHER, |option| &option.label)
                                .to_owned(),
                        );
                    }
                    let (note, _) = draft.editor.submission();
                    if !note.trim().is_empty() {
                        answers.push(format!("user_note: {}", note.trim()));
                    }
                }
                (question.id.clone(), json!({"answers":answers}))
            })
            .collect();
        Outcome::Answer(ApprovalDecision::Accept {
            content: Some(json!({"answers":answers})),
        })
    }

    fn advance(&mut self) -> Outcome {
        self.option_offset = 0;
        if self.current + 1 < self.questions.len() {
            self.current += 1;
        } else if self.unanswered() > 0 {
            self.confirmation = Some(0);
        } else {
            return self.response();
        }
        self.sync_focus();
        Outcome::Open
    }

    fn sync_focus(&mut self) {
        let draft = &mut self.drafts[self.current];
        match &self.questions[self.current].options {
            None => self.text_focus = true,
            Some(options) if draft.selected == Some(options.len()) => {
                draft.notes = true;
                self.text_focus = true;
            }
            Some(_) if !draft.notes => self.text_focus = false,
            _ => {}
        }
    }

    fn choose(&mut self, selected: usize) {
        let draft = &mut self.drafts[self.current];
        let count = self.questions[self.current]
            .options
            .as_ref()
            .map_or(0, Vec::len);
        let was_other = draft.selected == Some(count);
        draft.selected = Some(selected);
        draft.committed = false;
        if selected == count {
            draft.notes = true;
            self.text_focus = true;
        } else if was_other {
            self.text_focus = false;
            if draft.editor.is_empty() {
                draft.notes = false;
            }
        }
    }

    pub fn key(&mut self, key: KeyEvent) -> Outcome {
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        if control
            && key.code == KeyCode::Char('c')
            && self.confirmation.is_none()
            && self.text_focus
            && let Some(draft) = self.drafts.get_mut(self.current)
            && draft.notes
            && !draft.editor.is_empty()
        {
            draft.editor.clear();
            draft.committed = false;
            return Outcome::Open;
        }
        if control && key.code == KeyCode::Char('c') {
            return Outcome::Interrupt(ApprovalDecision::Cancel);
        }
        if self.questions.is_empty() {
            return Outcome::Answer(ApprovalDecision::Cancel);
        }
        if let Some(selected) = &mut self.confirmation {
            match key.code {
                KeyCode::Up | KeyCode::Down | KeyCode::Char('j' | 'k') => *selected = 1 - *selected,
                KeyCode::Char('1') => *selected = 0,
                KeyCode::Char('2') => *selected = 1,
                KeyCode::Enter if *selected == 0 => return self.response(),
                KeyCode::Enter | KeyCode::Backspace | KeyCode::Esc => {
                    self.confirmation = None;
                    self.option_offset = 0;
                    self.current = (0..self.questions.len())
                        .find(|&i| !self.answered(i))
                        .unwrap_or(0);
                    self.sync_focus();
                }
                _ => {}
            }
            return Outcome::Open;
        }
        let options = self.questions[self.current]
            .options
            .as_ref()
            .map_or(0, Vec::len);
        let notes = self.text_focus;
        if key.code == KeyCode::Esc {
            if self.drafts[self.current].notes && options > 0 {
                let draft = &mut self.drafts[self.current];
                draft.editor.clear();
                draft.committed = false;
                draft.notes = false;
                self.text_focus = false;
                return Outcome::Open;
            }
            return Outcome::Interrupt(ApprovalDecision::Cancel);
        }
        let previous = key.code == KeyCode::PageUp
            || (control && key.code == KeyCode::Char('p'))
            || (!notes && matches!(key.code, KeyCode::Left | KeyCode::Char('h')));
        let next = key.code == KeyCode::PageDown
            || (control && key.code == KeyCode::Char('n'))
            || (!notes && matches!(key.code, KeyCode::Right | KeyCode::Char('l')));
        if previous || next {
            self.option_offset = 0;
            self.current = (self.current + if next { 1 } else { self.questions.len() - 1 })
                % self.questions.len();
            self.sync_focus();
            return Outcome::Open;
        }
        if options > 0
            && (matches!(key.code, KeyCode::Up | KeyCode::Down)
                || (!notes && matches!(key.code, KeyCode::Char('j' | 'k'))))
        {
            let down = matches!(key.code, KeyCode::Down | KeyCode::Char('j'));
            let selected =
                self.drafts[self.current]
                    .selected
                    .unwrap_or(if down { options } else { 0 });
            self.choose((selected + if down { 1 } else { options }) % (options + 1));
            return Outcome::Open;
        }
        let draft = &mut self.drafts[self.current];
        match key.code {
            KeyCode::Tab if options > 0 && draft.selected.is_some() => {
                if notes {
                    draft.editor.clear();
                    draft.committed = false;
                }
                draft.notes = !notes;
                self.text_focus = !notes;
            }
            KeyCode::Enter if !notes && draft.selected == Some(options) => {
                draft.notes = true;
                self.text_focus = true;
            }
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) && notes => {
                draft.editor.newline();
                draft.committed = false;
            }
            KeyCode::Enter => {
                draft.committed = true;
                return self.advance();
            }
            KeyCode::Char(' ') if !notes => draft.committed = true,
            KeyCode::Backspace | KeyCode::Delete if !notes => {
                draft.selected = None;
                draft.editor.clear();
                draft.committed = false;
                draft.notes = false;
            }
            KeyCode::Backspace if notes && options > 0 && draft.editor.is_empty() => {
                draft.notes = false;
                self.text_focus = false;
            }
            KeyCode::Char(digit @ '1'..='9')
                if !notes && (digit as usize - '1' as usize) <= options =>
            {
                let selected = digit as usize - '1' as usize;
                self.choose(selected);
                if selected < options {
                    self.drafts[self.current].committed = true;
                    return self.advance();
                }
            }
            _ if notes => {
                match key.code {
                    KeyCode::Left => draft.editor.left(),
                    KeyCode::Right => draft.editor.right(),
                    KeyCode::Home => draft.editor.home(),
                    KeyCode::End => draft.editor.end(),
                    KeyCode::Backspace => draft.editor.backspace(),
                    KeyCode::Delete => draft.editor.delete_forward(),
                    KeyCode::Char('u') if control => draft.editor.delete_to_line_start(),
                    KeyCode::Char('w') if control => draft.editor.delete_word_backward(),
                    KeyCode::Char(c) if !control && !key.modifiers.contains(KeyModifiers::ALT) => {
                        draft.editor.insert(&c.to_string())
                    }
                    _ => return Outcome::Open,
                }
                draft.committed = false;
            }
            _ => {}
        }
        Outcome::Open
    }

    pub fn paste(&mut self, text: &str) {
        if self.confirmation.is_some() {
            return;
        }
        if let Some(draft) = self.drafts.get_mut(self.current) {
            draft.notes = true;
            draft.committed = false;
            self.text_focus = true;
            let text = crate::tools::plain(text);
            if crate::composer::should_collapse(&text) {
                draft.editor.insert_paste(&text);
            } else {
                draft.editor.insert(&text);
            }
        }
    }

    fn tips(&self) -> Vec<(&'static str, bool)> {
        let draft = &self.drafts[self.current];
        let has_options = self.questions[self.current].options.is_some();
        let mut tips = Vec::new();
        if has_options && draft.selected.is_some() {
            tips.push(if draft.notes {
                ("tab or esc to clear notes", false)
            } else {
                ("tab to add notes", true)
            });
        }
        let last = self.current + 1 == self.questions.len();
        tips.push((
            if last && self.questions.len() > 1 {
                "enter to submit all"
            } else {
                "enter to submit answer"
            },
            last,
        ));
        if self.questions.len() > 1 {
            tips.push((
                if self.text_focus {
                    "ctrl + p / ctrl + n change question"
                } else {
                    "←/→ to navigate questions"
                },
                false,
            ));
        }
        if !(has_options && draft.notes) {
            tips.push(("esc to interrupt", false));
        }
        tips
    }

    pub fn render(
        &mut self,
        width: usize,
        height: usize,
        theme: &Theme,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        if self.questions.is_empty() || width == 0 {
            return (vec![], None);
        }
        let height = height.max(1);
        let inner = width.saturating_sub(4).max(1);
        let cyan = Style::default().fg(ratatui_core::style::Color::Cyan);
        let unanswered = self.unanswered();
        let question = &self.questions[self.current];
        let draft = &self.drafts[self.current];
        let mut cursor = None;
        let mut lines = vec![Line::default()];
        if let Some(selected) = self.confirmation {
            let footer = footer(
                &[("Press enter to confirm or esc to go back", false)],
                inner,
                theme,
            );
            lines.push(Line::from(Span::styled(
                "Submit with unanswered questions?",
                Style::default().add_modifier(Modifier::BOLD),
            )));
            lines.push(Line::from(Span::styled(
                format!(
                    "{unanswered} unanswered question{}",
                    if unanswered == 1 { "" } else { "s" }
                ),
                theme.dim,
            )));
            lines.push(Line::default());
            let choices = option_rows(
                &[
                    (
                        "Proceed".into(),
                        format!(
                            "Submit with {unanswered} unanswered question{}.",
                            if unanswered == 1 { "" } else { "s" }
                        ),
                    ),
                    (
                        "Go back".into(),
                        "Return to the first unanswered question.".into(),
                    ),
                ],
                Some(selected),
                inner,
                theme,
            );
            let budget = height.saturating_sub(6 + footer.len()).max(1);
            if choices.iter().map(Vec::len).sum::<usize>() <= budget {
                lines.extend(choices.into_iter().flatten());
            } else {
                lines.extend(choices[selected].iter().take(budget).cloned());
            }
            lines.push(Line::default());
            lines.extend(footer);
        } else {
            let tips = self.tips();
            let mut footer = footer(&tips, inner, theme);
            let prompt_style = if self.answered(self.current) {
                Style::default()
            } else {
                cyan
            };
            let mut prompt = question
                .question
                .split('\n')
                .flat_map(|line| {
                    wrap::wrap(
                        &[Span::styled(crate::tools::plain(line), prompt_style)],
                        inner,
                        &[],
                        &[],
                    )
                })
                .collect::<Vec<_>>();
            let choices = question
                .options
                .as_ref()
                .map(|options| {
                    let choices = options
                        .iter()
                        .map(|option| (option.label.clone(), option.description.clone()))
                        .chain([(OTHER.into(), "Select to type your own answer.".into())])
                        .collect::<Vec<_>>();
                    option_rows(&choices, draft.selected, inner, theme)
                })
                .unwrap_or_default();
            let fixed = 6 + if draft.notes { 2 } else { 0 };
            if !choices.is_empty()
                && fixed + prompt.len() + choices.iter().map(Vec::len).sum::<usize>() + footer.len()
                    > height
            {
                let progress = format!(
                    "option {}/{}",
                    draft.selected.unwrap_or(0) + 1,
                    choices.len()
                );
                let mut scrolling_tips = vec![(progress.as_str(), false)];
                scrolling_tips.extend(tips);
                footer = self::footer(&scrolling_tips, inner, theme);
            }
            limit_lines(
                &mut prompt,
                height
                    .saturating_sub(fixed + footer.len() + usize::from(!choices.is_empty()))
                    .max(1),
                inner,
            );
            let pending = if unanswered > 0 {
                format!(" ({unanswered} unanswered)")
            } else {
                String::new()
            };
            lines.push(Line::from(Span::styled(
                format!(
                    "Question {}/{}{pending}",
                    self.current + 1,
                    self.questions.len()
                ),
                theme.dim,
            )));
            lines.extend(prompt);
            lines.push(Line::default());
            if !choices.is_empty() {
                let budget = height
                    .saturating_sub(lines.len() + footer.len() + if draft.notes { 4 } else { 2 })
                    .max(1);
                let selected = draft.selected.unwrap_or(0).min(choices.len() - 1);
                self.option_offset = self.option_offset.min(selected);
                while self.option_offset < selected
                    && choices[self.option_offset..=selected]
                        .iter()
                        .map(Vec::len)
                        .sum::<usize>()
                        > budget
                {
                    self.option_offset += 1;
                }
                let mut remaining = budget;
                for choice in &choices[self.option_offset..] {
                    if remaining == 0 {
                        break;
                    }
                    let mut visible = choice.clone();
                    limit_lines(&mut visible, remaining, inner);
                    remaining = remaining.saturating_sub(visible.len());
                    lines.extend(visible);
                }
            }
            if draft.notes {
                lines.push(Line::default());
                let placeholder = if question.options.is_none()
                    || draft.selected == question.options.as_ref().map(Vec::len)
                {
                    "Type your answer (optional)"
                } else if draft.selected.is_none() {
                    "Select an option to add notes"
                } else {
                    "Add notes"
                };
                let editor = &mut self.drafts[self.current].editor;
                let mut rendered = editor.render(
                    inner,
                    height
                        .saturating_sub(lines.len() + footer.len() + 2)
                        .clamp(1, 6),
                    Span::styled("› ", Style::default().add_modifier(Modifier::BOLD)),
                    theme.dim,
                );
                if editor.is_empty() {
                    rendered.lines[0]
                        .spans
                        .push(Span::styled(placeholder, theme.dim));
                }
                if self.text_focus {
                    cursor = Some((lines.len() + rendered.cursor.0, 2 + rendered.cursor.1));
                }
                lines.extend(rendered.lines);
            }
            lines.push(Line::default());
            lines.extend(footer);
        }
        lines.push(Line::default());
        // Remove spacing before sacrificing content in a short terminal.
        while lines.len() > height {
            let at = lines
                .iter()
                .position(|line| line.spans.iter().all(|span| span.content.trim().is_empty()))
                .unwrap_or(0);
            lines.remove(at);
            cursor = cursor.and_then(|(row, column)| {
                if row == at {
                    None
                } else {
                    Some((row - usize::from(row > at), column))
                }
            });
        }
        (
            lines
                .into_iter()
                .map(|line| {
                    let mut spans = vec![Span::raw("  ")];
                    spans.extend(line.spans);
                    surface(wrap::fit(spans, width, false), width, theme)
                })
                .collect(),
            cursor,
        )
    }
}

fn footer(tips: &[(&str, bool)], width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = vec![Line::default()];
    for (tip, highlight) in tips {
        let style = if *highlight {
            Style::default()
                .fg(ratatui_core::style::Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            theme.dim
        };
        let label = wrap::fit(vec![Span::styled((*tip).to_owned(), style)], width, true);
        let last = lines.len() - 1;
        let used = wrap::line_width(&lines[last]);
        if used > 0 && used + 3 + wrap::line_width(&label) > width {
            lines.push(label);
        } else {
            if used > 0 {
                lines[last].spans.push(Span::styled(" | ", theme.dim));
            }
            lines[last].spans.extend(label.spans);
        }
    }
    lines
}

fn option_rows(
    choices: &[(String, String)],
    selected: Option<usize>,
    width: usize,
    theme: &Theme,
) -> Vec<Vec<Line<'static>>> {
    let labels = choices
        .iter()
        .map(|(label, _)| crate::tools::plain(label))
        .collect::<Vec<_>>();
    let widest = labels
        .iter()
        .map(|label| wrap::width(label))
        .max()
        .unwrap_or(0);
    let aligned = width >= 56 && widest + 10 < width;
    choices
        .iter()
        .zip(labels)
        .enumerate()
        .map(|(index, ((_, description), label))| {
            let chosen = selected == Some(index);
            let style = if chosen {
                theme.accent.add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            let gap = if aligned {
                widest - wrap::width(&label) + 2
            } else {
                2
            };
            wrap::wrap(
                &[
                    Span::styled(label, style),
                    Span::styled(
                        format!("{}{}", " ".repeat(gap), crate::tools::plain(description)),
                        if chosen { style } else { theme.dim },
                    ),
                ],
                width,
                &[Span::styled(
                    format!("{} {}. ", if chosen { "›" } else { " " }, index + 1),
                    style,
                )],
                &[Span::styled("     ", style)],
            )
        })
        .collect()
}

fn limit_lines(lines: &mut Vec<Line<'static>>, maximum: usize, width: usize) {
    if lines.len() > maximum {
        lines.truncate(maximum);
        if let Some(last) = lines.last_mut() {
            let style = last
                .spans
                .last()
                .map_or(Style::default(), |span| span.style);
            *last = wrap::fit(
                std::mem::take(&mut last.spans),
                width.saturating_sub(1),
                false,
            );
            last.spans.push(Span::styled("…", style));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette::ColorLevel;

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn footer_keeps_tips_whole_and_highlights_only_important_tips() {
        let theme = Theme::fixture(ColorLevel::TrueColor, false);
        let lines = footer(
            &[
                ("tab to add notes", true),
                ("enter to submit all", true),
                ("←/→ to navigate questions", false),
                ("esc to interrupt", false),
            ],
            36,
            &theme,
        );
        assert_eq!(
            lines.iter().map(text).collect::<Vec<_>>(),
            [
                "tab to add notes",
                "enter to submit all",
                "←/→ to navigate questions",
                "esc to interrupt"
            ]
        );
        assert_eq!(
            lines[0].spans[0].style.fg,
            Some(ratatui_core::style::Color::Cyan)
        );
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[2].spans[0].style, theme.dim);
        let lines = footer(
            &[
                ("tab to add notes", true),
                ("enter to submit answer", false),
            ],
            76,
            &theme,
        );
        assert_eq!(text(&lines[0]), "tab to add notes | enter to submit answer");
    }

    #[test]
    fn options_align_descriptions_and_clip_with_a_visible_ellipsis() {
        let theme = Theme::fixture(ColorLevel::TrueColor, false);
        let choices = vec![
            ("Short".into(), "First description".into()),
            ("A longer choice".into(), "Second description".into()),
        ];
        let rows = option_rows(&choices, Some(1), 56, &theme);
        let column = |line: &Line<'_>, word: &str| {
            let text = text(line);
            wrap::width(&text[..text.find(word).expect("description")])
        };
        assert_eq!(column(&rows[0][0], "First"), column(&rows[1][0], "Second"));
        assert_eq!(
            rows[0][0].spans.last().expect("description").style,
            theme.dim
        );
        assert_eq!(
            rows[1][0].spans.last().expect("description").style,
            theme.accent.add_modifier(Modifier::BOLD)
        );
        let rows = option_rows(&choices, None, 24, &theme);
        assert!(text(&rows[1][1]).starts_with("     "));
        let mut rows = vec![Line::from("1234567890"), Line::from("continued")];
        limit_lines(&mut rows, 1, 10);
        assert_eq!(text(&rows[0]), "123456789…");
    }
}
