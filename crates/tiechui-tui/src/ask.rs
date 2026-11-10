//! Question drafts are local until an explicit submission.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui_core::{
    style::{Modifier, Style},
    text::{Line, Span},
};
use serde_json::{Value, json};
use tiechui_protocol::{ApprovalDecision, Question};

use crate::sheet::{self, List, Row};
use crate::tools::plain;
use crate::{composer::Composer, prompt::Outcome, theme::Theme, wrap};

/// What a custom answer is sent as; the list shows it as `Other`.
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

    fn hints(&self) -> Vec<(&'static str, &'static str)> {
        let draft = &self.drafts[self.current];
        let options = self.questions[self.current].options.is_some();
        let many = self.questions.len() > 1;
        let last = self.current + 1 == self.questions.len();
        let mut pairs = vec![("enter", if last && many { "submit all" } else { "answer" })];
        if options && draft.notes {
            pairs.push(("tab/esc", "clear note"));
        } else if options && draft.selected.is_some() {
            pairs.push(("tab", "add note"));
        }
        if many {
            pairs.push(if self.text_focus {
                ("ctrl+p/n", "question")
            } else {
                ("←→", "question")
            });
        }
        if !(options && draft.notes) {
            pairs.push(("esc", "interrupt"));
        }
        pairs
    }

    fn header(&self, index: usize) -> String {
        let header = plain(self.questions[index].header.trim());
        if header.is_empty() {
            format!("Question {}", index + 1)
        } else {
            header
        }
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
        let room = height.saturating_sub(3).max(1);
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let accent = theme.accent.add_modifier(Modifier::BOLD);
        let indent = [Span::raw("  ")];
        if let Some(selected) = self.confirmation {
            let missing: Vec<String> = (0..self.questions.len())
                .filter(|&i| !self.answered(i))
                .map(|i| self.header(i))
                .collect();
            let verb = if missing.len() == 1 { "has" } else { "have" };
            let mut body = wrap::wrap(
                &[
                    Span::styled(missing.join(", "), bold),
                    Span::styled(format!(" {verb} no answer yet."), theme.dim),
                ],
                width,
                &indent,
                &indent,
            );
            body.push(Line::default());
            let back = missing
                .first()
                .map_or_else(String::new, |h| format!(" to {h}"));
            let rows = [
                Row::new("Submit anyway", "", theme),
                Row::new(format!("Go back{back}"), "", theme),
            ];
            body.extend(sheet::options(&rows, selected, width, false, theme));
            body.drain(..body.len().saturating_sub(room));
            let meta = [vec![Span::styled(
                format!("{} unanswered", missing.len()),
                theme.dim,
            )]];
            let pairs = [("enter", "confirm"), ("1-2", "pick"), ("esc", "back")];
            return (
                sheet::frame(
                    theme.accent,
                    sheet::rule(
                        theme.accent,
                        &[sheet::title("Submit anyway?")],
                        &meta,
                        width,
                    ),
                    body,
                    sheet::hints(theme, &pairs, None, width),
                    width,
                ),
                None,
            );
        }
        let count = self.questions.len();
        let current = self.current;
        let mut titles = Vec::new();
        let mut metas = Vec::new();
        if count > 1 {
            let mut tabs = Vec::new();
            for index in 0..count {
                if index > 0 {
                    tabs.push(Span::raw("  "));
                }
                let header = self.header(index);
                if index == current {
                    tabs.push(Span::styled(header, accent));
                } else if self.answered(index) {
                    tabs.push(Span::styled(header, theme.dim));
                    tabs.push(Span::styled(" ✓", theme.success));
                } else {
                    tabs.push(Span::styled(header, theme.secondary));
                }
            }
            titles.push(tabs);
            metas.push(vec![Span::styled(
                format!("{} of {count}", current + 1),
                theme.dim,
            )]);
        }
        titles.push(vec![Span::styled(self.header(current), accent)]);
        let question = &self.questions[current];
        let mut prompt: Vec<Line<'static>> = question
            .question
            .split('\n')
            .flat_map(|line| {
                wrap::wrap(&[Span::styled(plain(line), bold)], width, &indent, &indent)
            })
            .collect();
        let rows: Vec<Row> = question
            .options
            .iter()
            .flatten()
            .map(|option| Row::new(plain(&option.label), plain(&option.description), theme))
            .chain(
                question
                    .options
                    .as_ref()
                    .map(|_| Row::new("Other", "Type your own answer", theme)),
            )
            .collect();
        let list = List::new(&rows, rows.len(), width, true);
        let draft = &self.drafts[current];
        let selected = draft
            .selected
            .unwrap_or(0)
            .min(rows.len().saturating_sub(1));
        let mut choices: Vec<Vec<Line<'static>>> = rows
            .iter()
            .enumerate()
            .map(|(index, row)| list.row(index, row, draft.selected == Some(index), width, theme))
            .collect();
        // Notes and custom answers sit under their option; text questions take them directly.
        let mut note = None;
        if draft.notes {
            let options = question.options.is_some();
            let lead = if !options {
                2
            } else if rows.len() <= 9 {
                7
            } else {
                4
            };
            let placeholder = if !options || draft.selected == Some(rows.len() - 1) {
                "Type your answer"
            } else {
                "Add a note"
            };
            let marker = if options {
                Span::styled("↳ ", theme.dim)
            } else {
                Span::styled("› ", bold)
            };
            let editor = &mut self.drafts[current].editor;
            let mut rendered = editor.render(
                width.saturating_sub(lead),
                room.saturating_sub(prompt.len() + 2).clamp(1, 6),
                marker,
                theme.dim,
            );
            if editor.is_empty() {
                rendered.lines[0]
                    .spans
                    .push(Span::styled(placeholder, theme.dim));
            }
            let lines: Vec<Line<'static>> = rendered
                .lines
                .into_iter()
                .map(|line| {
                    let mut spans = vec![Span::raw(" ".repeat(lead))];
                    spans.extend(line.spans);
                    Line::from(spans)
                })
                .collect();
            let (row, column) = rendered.cursor;
            if options {
                note = Some((choices[selected].len() + row, lead + column));
                choices[selected].extend(lines);
            } else {
                note = Some((row, lead + column));
                choices.push(lines);
            }
        }
        let total: usize = choices.iter().map(Vec::len).sum();
        let focus = if question.options.is_some() {
            selected
        } else {
            0
        };
        let keep = choices.get(focus).map_or(0, Vec::len);
        limit_lines(
            &mut prompt,
            room.saturating_sub(1 + total.min(room / 2).max(keep))
                .max(1),
            width,
        );
        let budget = room.saturating_sub(prompt.len() + 1).max(1);
        self.option_offset = self.option_offset.min(focus);
        while self.option_offset < focus
            && choices[self.option_offset..=focus]
                .iter()
                .map(Vec::len)
                .sum::<usize>()
                > budget
        {
            self.option_offset += 1;
        }
        let mut body = prompt;
        body.push(Line::default());
        let mut cursor = None;
        let mut remaining = budget;
        for (index, choice) in choices.iter().enumerate().skip(self.option_offset) {
            if remaining == 0 {
                break;
            }
            if index == focus
                && let Some((row, column)) = note
                && self.text_focus
                && row < remaining
            {
                cursor = Some((1 + body.len() + row, column));
            }
            let mut visible = choice.clone();
            limit_lines(&mut visible, remaining, width);
            remaining -= visible.len();
            body.extend(visible);
        }
        let position = (total > budget && !rows.is_empty())
            .then(|| format!("{} / {}", selected + 1, rows.len()));
        let lines = sheet::frame(
            theme.accent,
            sheet::rule(theme.accent, &titles, &metas, width),
            body,
            sheet::hints(theme, &self.hints(), position.as_deref(), width),
            width,
        );
        (lines, cursor)
    }
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

    fn text(line: &Line<'_>) -> String {
        line.spans
            .iter()
            .map(|span| span.content.as_ref())
            .collect()
    }

    #[test]
    fn limited_lines_end_with_a_visible_ellipsis() {
        let mut rows = vec![Line::from("1234567890"), Line::from("continued")];
        limit_lines(&mut rows, 1, 10);
        assert_eq!(text(&rows[0]), "123456789…");
    }
}
