//! Selection of the last painted alternate-screen text; live updates cannot change a copy.

use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui_core::{
    style::Modifier,
    text::{Line, Span},
};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

type Point = (usize, usize);

#[derive(Default)]
pub struct Selection {
    painted: Vec<Line<'static>>,
    held: Option<Vec<Line<'static>>>,
    anchor: Point,
    end: Point,
    pressed: Option<MouseEvent>,
    dragged: bool,
    last_click: Option<(Point, Instant, u8)>,
    copy: Option<String>,
}

pub enum MouseAction {
    Handled,
    Pass,
    Click(MouseEvent),
}

impl Selection {
    pub fn active(&self) -> bool {
        self.held.is_some()
    }

    pub fn clear(&mut self) {
        self.held = None;
        self.pressed = None;
        self.last_click = None;
        self.copy = None;
    }

    pub fn take_copy(&mut self) -> Option<String> {
        self.copy.take()
    }

    fn text(&self, row: usize) -> String {
        self.held
            .as_ref()
            .unwrap_or(&self.painted)
            .get(row)
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn point(&self, column: usize, row: usize) -> Point {
        let row = row.min(self.painted.len().saturating_sub(1));
        let text = self.text(row);
        let mut x = 0;
        let mut index = 0;
        for grapheme in text.graphemes(true) {
            if x + wrap::width(grapheme) > column {
                break;
            }
            x += wrap::width(grapheme);
            index += 1;
        }
        (row, index)
    }

    pub fn mouse(&mut self, event: MouseEvent, now: Instant, copy_on_select: bool) -> MouseAction {
        let point = self.point(usize::from(event.column), usize::from(event.row));
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) if !self.painted.is_empty() => {
                let clicks = self
                    .last_click
                    .filter(|(last, at, _)| {
                        *last == point && now.duration_since(*at) < Duration::from_millis(400)
                    })
                    .map_or(1, |(_, _, count)| count % 3 + 1);
                self.last_click = Some((point, now, clicks));
                if self.held.is_none() || !event.modifiers.contains(KeyModifiers::SHIFT) {
                    self.held = Some(self.painted.clone());
                    self.anchor = point;
                }
                self.end = point;
                self.pressed = Some(event);
                self.dragged = false;
                let text = self.text(point.0);
                let graphemes: Vec<_> = text.graphemes(true).collect();
                if clicks == 3 {
                    self.anchor.1 = 0;
                    self.end.1 = graphemes.len();
                } else if clicks == 2 && point.1 < graphemes.len() {
                    let word = |g: &str| g.chars().any(|ch| ch.is_alphanumeric() || ch == '_');
                    let kind = word(graphemes[point.1]);
                    while self.anchor.1 > 0 && word(graphemes[self.anchor.1 - 1]) == kind {
                        self.anchor.1 -= 1;
                    }
                    while self.end.1 < graphemes.len() && word(graphemes[self.end.1]) == kind {
                        self.end.1 += 1;
                    }
                }
                MouseAction::Handled
            }
            MouseEventKind::Drag(MouseButton::Left) if self.pressed.is_some() => {
                self.dragged = true;
                self.end = point;
                MouseAction::Handled
            }
            MouseEventKind::Up(MouseButton::Left) if self.pressed.is_some() => {
                let Some(pressed) = self.pressed.take() else {
                    return MouseAction::Pass;
                };
                // A stationary release preserves the word or line expanded on press.
                if self.dragged || (event.column, event.row) != (pressed.column, pressed.row) {
                    self.end = point;
                }
                if !self.dragged && self.anchor == self.end {
                    self.held = None;
                    return MouseAction::Click(pressed);
                }
                if copy_on_select {
                    self.request_copy();
                }
                MouseAction::Handled
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.clear();
                MouseAction::Pass
            }
            _ => MouseAction::Pass,
        }
    }

    fn request_copy(&mut self) {
        self.copy = self.selected_text().filter(|text| !text.trim().is_empty());
    }

    pub fn selected_text(&self) -> Option<String> {
        self.held.as_ref()?;
        if self.anchor == self.end {
            return None;
        }
        let (start, end) = (self.anchor.min(self.end), self.anchor.max(self.end));
        let mut rows = Vec::new();
        for row in start.0..=end.0 {
            let text = self.text(row);
            let from = if row == start.0 { start.1 } else { 0 };
            let to = if row == end.0 { end.1 } else { usize::MAX };
            let selected: String = text
                .graphemes(true)
                .skip(from)
                .take(to.saturating_sub(from))
                .collect();
            rows.push(
                selected
                    .trim_end()
                    .chars()
                    .filter(|ch| !ch.is_control() || *ch == '\t')
                    .collect::<String>(),
            );
        }
        Some(rows.join("\n"))
    }

    pub fn key(&mut self, key: KeyEvent) -> bool {
        if self.held.is_none() {
            return false;
        }
        match key.code {
            KeyCode::Esc => self.clear(),
            KeyCode::Enter => self.request_copy(),
            KeyCode::Char('c' | 'C')
                if key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER) =>
            {
                self.request_copy()
            }
            KeyCode::Left => {
                if self.end.1 > 0 {
                    self.end.1 -= 1;
                } else if self.end.0 > 0 {
                    self.end.0 -= 1;
                    self.end.1 = self.text(self.end.0).graphemes(true).count();
                }
            }
            KeyCode::Right => {
                if self.end.1 < self.text(self.end.0).graphemes(true).count() {
                    self.end.1 += 1;
                } else if self.end.0 + 1 < self.painted.len() {
                    self.end = (self.end.0 + 1, 0);
                }
            }
            KeyCode::Up | KeyCode::Down => {
                let column: usize = self
                    .text(self.end.0)
                    .graphemes(true)
                    .take(self.end.1)
                    .map(wrap::width)
                    .sum();
                self.end = self.point(
                    column,
                    if key.code == KeyCode::Up {
                        self.end.0.saturating_sub(1)
                    } else {
                        self.end.0 + 1
                    },
                );
            }
            _ => {
                self.clear();
                return false;
            }
        }
        true
    }

    pub fn frame(&mut self, frame: Vec<Line<'static>>) -> Vec<Line<'static>> {
        let Some(held) = &self.held else {
            self.painted = frame.clone();
            return frame;
        };
        let (start, end) = (self.anchor.min(self.end), self.anchor.max(self.end));
        held.iter()
            .enumerate()
            .map(|(row, line)| {
                let mut column = 0;
                let spans = line
                    .spans
                    .iter()
                    .flat_map(|span| {
                        span.content
                            .graphemes(true)
                            .map(|grapheme| {
                                let point = (row, column);
                                column += 1;
                                let style = line.style.patch(span.style);
                                Span::styled(
                                    grapheme.to_owned(),
                                    if point >= start && point < end {
                                        style.add_modifier(Modifier::REVERSED)
                                    } else {
                                        style
                                    },
                                )
                            })
                            .collect::<Vec<_>>()
                    })
                    .collect::<Vec<_>>();
                Line::from(spans)
            })
            .collect()
    }
}
