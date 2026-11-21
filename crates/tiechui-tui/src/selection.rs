//! Selection of painted panel text or frozen scrollable document lines.

use crate::wrap;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui_core::{
    style::Modifier,
    text::{Line, Span},
};
use std::time::{Duration, Instant};
use unicode_segmentation::UnicodeSegmentation;

type Point = (usize, usize);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DocSource {
    Main,
    Pager,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    Doc(DocSource),
    Panel(u64),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Viewport {
    pub top: usize,
    pub height: usize,
    pub rows: usize,
}

impl Viewport {
    pub fn contains_row(self, row: usize) -> bool {
        row >= self.top && row < self.top + self.height
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DocView {
    pub source: DocSource,
    pub viewport: Viewport,
    pub offset: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct DocState {
    source: DocSource,
    pointer: (usize, usize),
    left_top: bool,
    left_bottom: bool,
    edge_scroll: Option<isize>,
}

impl DocState {
    fn at_top_edge(self, vp: Viewport) -> bool {
        let row = self.pointer.1;
        if vp.top > 0 {
            row < vp.top
        } else {
            row == 0 && self.left_top
        }
    }

    fn at_bottom_edge(self, vp: Viewport) -> bool {
        let row = self.pointer.1;
        let bottom = vp.top + vp.height;
        if bottom < vp.rows {
            row >= bottom
        } else {
            row + 1 >= vp.rows && self.left_bottom
        }
    }

    fn edge_delta(self, vp: Viewport) -> Option<isize> {
        if self.at_top_edge(vp) {
            Some(-2)
        } else {
            self.at_bottom_edge(vp).then_some(2)
        }
    }

    fn step(self, vp: Viewport, offset: usize, max_offset: usize) -> Option<isize> {
        let step = self.edge_scroll?;
        let active = if step < 0 {
            self.at_top_edge(vp) && offset > 0
        } else {
            self.at_bottom_edge(vp) && offset < max_offset
        };
        active.then_some(step)
    }
}

enum Held {
    Panel {
        id: u64,
        lines: Vec<Line<'static>>,
    },
    Doc {
        lines: Vec<Line<'static>>,
        state: DocState,
    },
}

#[derive(Default)]
pub struct Selection {
    painted: Vec<Line<'static>>,
    held: Option<Held>,
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

fn is_word_grapheme(grapheme: &str) -> bool {
    grapheme.chars().any(|ch| ch.is_alphanumeric() || ch == '_')
}

fn expand_word(graphemes: &[&str], at: usize, start: &mut usize, end: &mut usize) {
    let kind = is_word_grapheme(graphemes[at]);
    while *start > 0 && is_word_grapheme(graphemes[*start - 1]) == kind {
        *start -= 1;
    }
    while *end < graphemes.len() && is_word_grapheme(graphemes[*end]) == kind {
        *end += 1;
    }
}

fn highlight_line(line: &Line<'static>, row: usize, start: Point, end: Point) -> Line<'static> {
    if start == end || row < start.0 || row > end.0 {
        return line.clone();
    }
    let mut column = 0;
    let spans: Vec<_> = line
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
        .collect();
    Line::from(spans)
}

impl Selection {
    pub fn active(&self) -> bool {
        self.held.is_some()
    }

    pub fn surface(&self) -> Option<Surface> {
        match &self.held {
            Some(Held::Panel { id, .. }) => Some(Surface::Panel(*id)),
            Some(Held::Doc { state, .. }) => Some(Surface::Doc(state.source)),
            None => None,
        }
    }

    pub fn doc_source(&self) -> Option<DocSource> {
        match &self.held {
            Some(Held::Doc { state, .. }) => Some(state.source),
            _ => None,
        }
    }

    pub fn doc_lines(&self, source: DocSource) -> Option<&[Line<'static>]> {
        match &self.held {
            Some(Held::Doc { lines, state }) if state.source == source => Some(lines),
            _ => None,
        }
    }

    pub fn doc_len(&self) -> usize {
        match &self.held {
            Some(Held::Doc { lines, .. }) => lines.len(),
            _ => 0,
        }
    }

    pub fn end_row(&self) -> Option<usize> {
        self.held.as_ref().map(|_| self.end.0)
    }

    pub fn clear(&mut self) {
        self.held = None;
        self.pressed = None;
        self.dragged = false;
        self.last_click = None;
        self.copy = None;
    }

    pub fn take_copy(&mut self) -> Option<String> {
        self.copy.take()
    }

    fn lines(&self) -> &[Line<'static>] {
        match &self.held {
            Some(Held::Panel { lines, .. } | Held::Doc { lines, .. }) => lines,
            None => &self.painted,
        }
    }

    fn line_count(&self) -> usize {
        self.lines().len()
    }

    fn text(&self, row: usize) -> String {
        self.lines()
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
        let row = row.min(self.line_count().saturating_sub(1));
        let text = self.text(row);
        let mut x = 0usize;
        let mut index = 0;
        for grapheme in text.graphemes(true) {
            let w = wrap::width(grapheme);
            if x.saturating_add(w) > column {
                break;
            }
            x = x.saturating_add(w);
            index += 1;
        }
        (row, index)
    }

    fn doc_point(&self, column: usize, row: usize, viewport: Viewport, offset: usize) -> Point {
        let count = self.line_count();
        if count == 0 || viewport.height == 0 {
            return (0, 0);
        }
        if row < viewport.top {
            return (offset.min(count - 1), 0);
        }
        if row >= viewport.top + viewport.height {
            let last = (offset + viewport.height - 1).min(count - 1);
            return self.point(usize::MAX, last);
        }
        let doc_row = offset + (row - viewport.top);
        if doc_row >= count {
            return self.point(usize::MAX, count - 1);
        }
        self.point(column, doc_row)
    }

    fn extend_to(&mut self, point: Point) {
        let (start, end) = (self.anchor.min(self.end), self.anchor.max(self.end));
        if point < start {
            self.anchor = end;
        } else if point > end {
            self.anchor = start;
        }
        self.end = point;
    }

    fn expand_multi_click(&mut self, point: Point, clicks: u8) {
        if clicks < 2 {
            return;
        }
        let text = self.text(point.0);
        let graphemes: Vec<_> = text.graphemes(true).collect();
        if clicks == 3 {
            self.anchor.1 = 0;
            self.end.1 = graphemes.len();
        } else if point.1 < graphemes.len() {
            expand_word(&graphemes, point.1, &mut self.anchor.1, &mut self.end.1);
        }
    }

    fn begin_press(&mut self, event: MouseEvent, point: Point, now: Instant, extend: bool) {
        self.copy = None;
        let clicks = if extend {
            1
        } else {
            self.last_click
                .filter(|(last, at, _)| {
                    *last == point && now.duration_since(*at) < Duration::from_millis(400)
                })
                .map_or(1, |(_, _, count)| count % 3 + 1)
        };
        self.last_click = Some((point, now, clicks));
        if extend {
            self.extend_to(point);
        } else {
            self.anchor = point;
            self.end = point;
            self.expand_multi_click(point, clicks);
        }
        self.pressed = Some(event);
        self.dragged = false;
    }

    fn finish_release(
        &mut self,
        event: MouseEvent,
        point: Point,
        copy_on_select: bool,
    ) -> MouseAction {
        let Some(pressed) = self.pressed.take() else {
            return MouseAction::Pass;
        };
        let moved = std::mem::take(&mut self.dragged)
            || (event.column, event.row) != (pressed.column, pressed.row);
        if moved {
            self.end = point;
        }
        if self.anchor == self.end {
            self.held = None;
            if moved {
                self.last_click = None;
                return MouseAction::Handled;
            }
            return MouseAction::Click(pressed);
        }
        if copy_on_select {
            self.request_copy();
        }
        MouseAction::Handled
    }

    fn pressed_in(&self, source: DocSource) -> bool {
        self.pressed.is_some() && self.doc_source() == Some(source)
    }

    fn record_pointer(&mut self, col: usize, row: usize, vp: Viewport, dragging: bool) {
        if let Some(Held::Doc { state, .. }) = &mut self.held {
            state.pointer = (col, row);
            state.left_top |= row > 0;
            state.left_bottom |= row + 1 < vp.rows;
            state.edge_scroll = dragging.then(|| state.edge_delta(vp)).flatten();
        }
    }

    fn doc_down(
        &mut self,
        button: MouseButton,
        event: MouseEvent,
        now: Instant,
        view: DocView,
        snapshot: impl FnOnce() -> Vec<Line<'static>>,
    ) -> MouseAction {
        let (col, row) = (usize::from(event.column), usize::from(event.row));
        if !view.viewport.contains_row(row) {
            self.clear();
            return MouseAction::Pass;
        }
        if button != MouseButton::Left {
            return MouseAction::Pass;
        }
        let next_state = DocState {
            source: view.source,
            pointer: (col, row),
            left_top: row > 0,
            left_bottom: row + 1 < view.viewport.rows,
            edge_scroll: None,
        };
        let reused = if let Some(Held::Doc { state, .. }) = &mut self.held
            && state.source == view.source
        {
            *state = next_state;
            true
        } else {
            let lines = snapshot();
            if lines.is_empty() {
                self.clear();
                return MouseAction::Pass;
            }
            self.held = Some(Held::Doc {
                lines,
                state: next_state,
            });
            false
        };
        let extend = reused && event.modifiers.contains(KeyModifiers::SHIFT);
        let point = self.doc_point(col, row, view.viewport, view.offset);
        self.begin_press(event, point, now, extend);
        MouseAction::Handled
    }

    fn doc_scroll(
        &mut self,
        source: DocSource,
        pointer: Point,
        delta: isize,
        scroll: &mut dyn FnMut(isize) -> (Viewport, usize),
    ) -> MouseAction {
        if self.doc_lines(source).is_none() {
            return MouseAction::Pass;
        }
        let (viewport, offset) = scroll(delta);
        if self.pressed.is_some() {
            self.dragged = true;
            self.record_pointer(pointer.0, pointer.1, viewport, false);
            self.end = self.doc_point(pointer.0, pointer.1, viewport, offset);
        }
        MouseAction::Handled
    }

    pub fn doc_mouse(
        &mut self,
        event: MouseEvent,
        now: Instant,
        copy_on_select: bool,
        view: DocView,
        snapshot: impl FnOnce() -> Vec<Line<'static>>,
        mut scroll: impl FnMut(isize) -> (Viewport, usize),
    ) -> MouseAction {
        let (col, row) = (usize::from(event.column), usize::from(event.row));
        match event.kind {
            MouseEventKind::Down(button) => self.doc_down(button, event, now, view, snapshot),
            MouseEventKind::Drag(MouseButton::Left) if self.pressed_in(view.source) => {
                self.dragged = true;
                self.record_pointer(col, row, view.viewport, true);
                self.end = self.doc_point(col, row, view.viewport, view.offset);
                MouseAction::Handled
            }
            MouseEventKind::Up(MouseButton::Left) if self.pressed_in(view.source) => {
                let point = self.doc_point(col, row, view.viewport, view.offset);
                self.finish_release(event, point, copy_on_select)
            }
            MouseEventKind::ScrollUp => self.doc_scroll(view.source, (col, row), -3, &mut scroll),
            MouseEventKind::ScrollDown => self.doc_scroll(view.source, (col, row), 3, &mut scroll),
            _ => MouseAction::Pass,
        }
    }

    pub fn autoscroll_step(
        &self,
        viewport: Viewport,
        offset: usize,
        max_offset: usize,
    ) -> Option<isize> {
        self.pressed?;
        let Some(Held::Doc { state, .. }) = &self.held else {
            return None;
        };
        state.step(viewport, offset, max_offset)
    }

    pub fn apply_autoscroll(&mut self, viewport: Viewport, offset: usize) {
        if self.pressed.is_none() {
            return;
        }
        if let Some(Held::Doc { state, .. }) = &self.held {
            let (col, row) = state.pointer;
            self.end = self.doc_point(col, row, viewport, offset);
        }
    }

    pub fn highlight_slice(&self, slice: Vec<Line<'static>>, offset: usize) -> Vec<Line<'static>> {
        if self.held.is_none() || self.anchor == self.end {
            return slice;
        }
        let (start, end) = (self.anchor.min(self.end), self.anchor.max(self.end));
        slice
            .iter()
            .enumerate()
            .map(|(index, line)| highlight_line(line, offset + index, start, end))
            .collect()
    }

    fn mouse_down(
        &mut self,
        id: u64,
        event: MouseEvent,
        point: Point,
        now: Instant,
    ) -> MouseAction {
        if self.painted.is_empty() {
            return MouseAction::Pass;
        }
        let extend = matches!(self.held, Some(Held::Panel { id: held_id, .. }) if held_id == id)
            && event.modifiers.contains(KeyModifiers::SHIFT);
        if !extend {
            self.held = Some(Held::Panel {
                id,
                lines: self.painted.clone(),
            });
        }
        self.begin_press(event, point, now, extend);
        MouseAction::Handled
    }

    pub fn panel_mouse(
        &mut self,
        id: u64,
        event: MouseEvent,
        now: Instant,
        copy_on_select: bool,
    ) -> MouseAction {
        let point = self.point(usize::from(event.column), usize::from(event.row));
        match event.kind {
            MouseEventKind::Down(MouseButton::Left) => self.mouse_down(id, event, point, now),
            MouseEventKind::Drag(MouseButton::Left) if self.pressed.is_some() => {
                self.dragged = true;
                self.end = point;
                MouseAction::Handled
            }
            MouseEventKind::Up(MouseButton::Left) if self.pressed.is_some() => {
                self.finish_release(event, point, copy_on_select)
            }
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown => {
                self.clear();
                MouseAction::Pass
            }
            _ => MouseAction::Pass,
        }
    }

    pub fn mouse(&mut self, event: MouseEvent, now: Instant, copy_on_select: bool) -> MouseAction {
        self.panel_mouse(0, event, now, copy_on_select)
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

    fn step_end(&mut self, code: KeyCode) -> bool {
        match code {
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
                } else if self.end.0 + 1 < self.line_count() {
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
                let next_row = if code == KeyCode::Up {
                    self.end.0.saturating_sub(1)
                } else {
                    self.end.0 + 1
                };
                self.end = self.point(column, next_row);
            }
            _ => return false,
        }
        true
    }

    pub fn key(&mut self, key: KeyEvent) -> bool {
        if !self.active() {
            return false;
        }
        if !key.modifiers.is_empty() {
            if matches!(key.code, KeyCode::Char('c' | 'C'))
                && key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::SUPER)
                && !key.modifiers.contains(KeyModifiers::ALT)
            {
                self.request_copy();
                return true;
            }
            self.clear();
            return false;
        }
        if key.code == KeyCode::Esc {
            self.clear();
        } else if key.code == KeyCode::Enter {
            self.request_copy();
        } else if !self.step_end(key.code) {
            self.clear();
            return false;
        }
        true
    }

    pub fn frame(&mut self, frame: Vec<Line<'static>>) -> Vec<Line<'static>> {
        let Some(Held::Panel { lines: held, .. }) = &self.held else {
            self.painted = frame.clone();
            return frame;
        };
        let (start, end) = (self.anchor.min(self.end), self.anchor.max(self.end));
        held.iter()
            .enumerate()
            .map(|(row, line)| highlight_line(line, row, start, end))
            .collect()
    }
}
