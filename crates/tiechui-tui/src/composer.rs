//! Multi-line composer: grapheme editing with atomic paste and image elements.

use std::ops::Range;

use ratatui_core::style::Style;
use ratatui_core::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;

use crate::wrap;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Payload {
    Paste(String),
    Reference(String),
    Image { mime_type: String, data: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Element {
    range: Range<usize>,
    payload: Payload,
}

/// Text plus the elements inside it; ranges are relative to `text`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Fragment {
    text: String,
    elements: Vec<Element>,
}

impl Fragment {
    fn text(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            elements: Vec::new(),
        }
    }

    fn element(label: String, payload: Payload) -> Self {
        Self {
            elements: vec![Element {
                range: 0..label.len(),
                payload,
            }],
            text: label,
        }
    }
}

/// Everything needed to restore an unsent input exactly.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Draft {
    fragment: Fragment,
    cursor: usize,
}

impl Draft {
    /// Rebuilds a draft from submitted parts: long text recollapses and `[Image #N]` labels
    /// rebind to their images in order; images without a label are appended.
    pub fn from_parts(text: &str, images: Vec<(String, String)>) -> Self {
        let mut fragment = if images.is_empty() && should_collapse(text) {
            Fragment::element(
                format!("[Pasted Content {} chars]", text.chars().count()),
                Payload::Paste(text.to_owned()),
            )
        } else {
            Fragment::text(text)
        };
        let mut from = 0;
        for (number, (mime_type, data)) in images.into_iter().enumerate() {
            let label = format!("[Image #{}]", number + 1);
            let payload = Payload::Image { mime_type, data };
            let range = match fragment.text[from..].find(&label) {
                Some(at) => from + at..from + at + label.len(),
                None => {
                    if !fragment.text.is_empty() && !fragment.text.ends_with(char::is_whitespace) {
                        fragment.text.push(' ');
                    }
                    let start = fragment.text.len();
                    fragment.text.push_str(&label);
                    start..fragment.text.len()
                }
            };
            from = range.end;
            fragment.elements.push(Element { range, payload });
        }
        Self {
            cursor: fragment.text.len(),
            fragment,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.fragment.text.is_empty()
    }

    pub fn text(&self) -> &str {
        &self.fragment.text
    }

    /// Joins drafts with newlines, keeping every element; the cursor ends up at the end.
    pub fn join(drafts: impl IntoIterator<Item = Draft>) -> Self {
        let mut joined = Fragment::default();
        for draft in drafts.into_iter().filter(|d| !d.is_empty()) {
            if !joined.text.is_empty() {
                joined.text.push('\n');
            }
            let offset = joined.text.len();
            joined.text.push_str(&draft.fragment.text);
            joined
                .elements
                .extend(draft.fragment.elements.into_iter().map(|e| Element {
                    range: e.range.start + offset..e.range.end + offset,
                    payload: e.payload,
                }));
        }
        Self {
            cursor: joined.text.len(),
            fragment: joined,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Edit {
    Typing,
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Space,
    Word,
    Punctuation,
}

fn class(grapheme: &str) -> Class {
    match grapheme.chars().next() {
        Some(c) if c.is_whitespace() => Class::Space,
        Some(c) if c.is_alphanumeric() || c == '_' => Class::Word,
        _ => Class::Punctuation,
    }
}

const PASTE_CHARS: usize = 1000;
const PASTE_NEWLINES: usize = 2;
const UNDO_LIMIT: usize = 200;

pub fn should_collapse(text: &str) -> bool {
    text.chars().count() > PASTE_CHARS || text.matches('\n').count() > PASTE_NEWLINES
}

#[derive(Debug, Default)]
pub struct Composer {
    escape: Option<std::time::Instant>,
    draft: Draft,
    undo: Vec<Draft>,
    last_edit: Option<Edit>,
    kills: Vec<Fragment>,
    /// Range of the last yank and which kill it came from, for yank-pop.
    yanked: Option<(Range<usize>, usize)>,
    column: Option<usize>,
    scroll: usize,
    revision: u64,
}

pub struct Rendered {
    pub lines: Vec<Line<'static>>,
    pub cursor: (usize, usize),
    pub above: usize,
    pub below: usize,
}

impl Composer {
    pub fn image_count(&self) -> usize {
        self.elements()
            .iter()
            .filter(|element| matches!(element.payload, Payload::Image { .. }))
            .count()
    }
    pub fn reset_escape(&mut self) {
        self.escape = None;
    }
    pub fn escape_pending(&self) -> bool {
        self.escape.is_some()
    }
    pub fn expire_escape(&mut self, now: std::time::Instant) {
        if self.escape.is_some_and(|last| {
            now.saturating_duration_since(last) >= std::time::Duration::from_millis(500)
        }) {
            self.escape = None;
        }
    }
    pub fn escape(&mut self, now: std::time::Instant) -> bool {
        self.expire_escape(now);
        if self.escape.take().is_some() {
            true
        } else {
            self.escape = Some(now);
            false
        }
    }
    pub fn detach_images(&mut self) {
        let (text, _) = self.submission_without_image_labels();
        self.clear();
        self.set_text(&text);
    }
    pub fn is_empty(&self) -> bool {
        self.draft.fragment.text.is_empty()
    }

    pub fn text(&self) -> &str {
        &self.draft.fragment.text
    }

    /// Changes whenever the content changes, so callers can detect edits.
    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn at_boundary(&self) -> bool {
        self.draft.cursor == 0 || self.draft.cursor == self.text().len()
    }

    pub fn draft(&self) -> Draft {
        self.draft.clone()
    }

    pub fn cursor(&self) -> usize {
        self.draft.cursor
    }

    pub fn restore(&mut self, draft: Draft) {
        self.checkpoint(Edit::Other);
        self.draft = draft;
        self.renumber_images();
        self.changed();
    }

    pub fn clear(&mut self) -> Draft {
        let draft = std::mem::take(&mut self.draft);
        self.undo.clear();
        self.last_edit = None;
        self.yanked = None;
        self.changed();
        draft
    }

    /// Replaces the content with `text` and puts the cursor at the end, recollapsing long text.
    pub fn set_text(&mut self, text: &str) {
        self.clear();
        if should_collapse(text) {
            self.insert_paste(text);
        } else {
            self.insert_fragment(Fragment::text(text), Edit::Other);
        }
    }

    fn changed(&mut self) {
        self.revision += 1;
        self.column = None;
    }

    fn checkpoint(&mut self, edit: Edit) {
        if !(edit == Edit::Typing && self.last_edit == Some(Edit::Typing)) {
            self.undo.push(self.draft.clone());
            if self.undo.len() > UNDO_LIMIT {
                self.undo.remove(0);
            }
        }
        self.last_edit = Some(edit);
        self.yanked = None;
    }

    pub fn undo(&mut self) {
        if let Some(draft) = self.undo.pop() {
            self.draft = draft;
            self.last_edit = None;
            self.yanked = None;
            self.changed();
        }
    }

    fn elements(&self) -> &[Element] {
        &self.draft.fragment.elements
    }

    /// Grows a range so it never cuts an element; an empty range inside one moves to its end.
    fn expand(&self, mut range: Range<usize>) -> Range<usize> {
        for element in self.elements() {
            let e = &element.range;
            if range.is_empty() {
                if e.start < range.start && range.start < e.end {
                    range = e.end..e.end;
                }
            } else if range.start < e.end && range.end > e.start {
                range.start = range.start.min(e.start);
                range.end = range.end.max(e.end);
            }
        }
        range
    }

    fn extract(&self, range: &Range<usize>) -> Fragment {
        Fragment {
            text: self.text()[range.clone()].to_owned(),
            elements: self
                .elements()
                .iter()
                .filter(|e| e.range.start >= range.start && e.range.end <= range.end)
                .map(|e| Element {
                    range: e.range.start - range.start..e.range.end - range.start,
                    payload: e.payload.clone(),
                })
                .collect(),
        }
    }

    /// The only mutation primitive: replaces `range` (grown to element bounds) with `fragment`.
    fn replace(&mut self, range: Range<usize>, fragment: Fragment) -> Fragment {
        let range = self.expand(range);
        let removed = self.extract(&range);
        let inserted = fragment.text.len();
        let current = &mut self.draft.fragment;
        current.text.replace_range(range.clone(), &fragment.text);
        current
            .elements
            .retain(|e| e.range.end <= range.start || e.range.start >= range.end);
        for element in &mut current.elements {
            if element.range.start >= range.end {
                element.range.start = element.range.start - range.end + range.start + inserted;
                element.range.end = element.range.end - range.end + range.start + inserted;
            }
        }
        current
            .elements
            .extend(fragment.elements.into_iter().map(|e| Element {
                range: e.range.start + range.start..e.range.end + range.start,
                payload: e.payload,
            }));
        current.elements.sort_by_key(|e| e.range.start);
        self.draft.cursor = range.start + inserted;
        self.renumber_images();
        self.changed();
        removed
    }

    fn renumber_images(&mut self) {
        let mut number = 0;
        let mut index = 0;
        while index < self.draft.fragment.elements.len() {
            let element = &self.draft.fragment.elements[index];
            if matches!(element.payload, Payload::Image { .. }) {
                number += 1;
                let label = format!("[Image #{number}]");
                let range = element.range.clone();
                if self.draft.fragment.text[range.clone()] != label {
                    let delta = label.len() as isize - range.len() as isize;
                    self.draft
                        .fragment
                        .text
                        .replace_range(range.clone(), &label);
                    for (other, element) in self.draft.fragment.elements.iter_mut().enumerate() {
                        if other == index {
                            element.range.end = range.start + label.len();
                        } else if element.range.start >= range.end {
                            element.range.start = (element.range.start as isize + delta) as usize;
                            element.range.end = (element.range.end as isize + delta) as usize;
                        }
                    }
                    if self.draft.cursor >= range.end {
                        self.draft.cursor = (self.draft.cursor as isize + delta) as usize;
                    }
                }
            }
            index += 1;
        }
    }

    fn insert_fragment(&mut self, fragment: Fragment, edit: Edit) {
        self.checkpoint(edit);
        let at = self.expand(self.draft.cursor..self.draft.cursor);
        self.replace(at, fragment);
    }

    pub fn insert(&mut self, text: &str) {
        let edit = if text.chars().all(|c| !c.is_whitespace()) && text.chars().count() == 1 {
            Edit::Typing
        } else {
            Edit::Other
        };
        self.insert_fragment(Fragment::text(text), edit);
    }

    pub fn newline(&mut self) {
        self.insert_fragment(Fragment::text("\n"), Edit::Other);
    }

    /// Long pastes become one atomic `[Pasted Content N chars]` element numbered among equals.
    pub fn insert_paste(&mut self, text: &str) {
        let base = format!("[Pasted Content {} chars", text.chars().count());
        let mut label = format!("{base}]");
        let mut number = 1;
        while self.text().contains(&label) {
            number += 1;
            label = format!("{base} #{number}]");
        }
        self.insert_fragment(
            Fragment::element(label, Payload::Paste(text.to_owned())),
            Edit::Other,
        );
    }

    pub fn insert_image(&mut self, mime_type: String, data: String) {
        self.insert_fragment(
            Fragment::element("[Image]".into(), Payload::Image { mime_type, data }),
            Edit::Other,
        );
    }

    pub fn replace_reference(&mut self, range: Range<usize>, label: String, reference: String) {
        self.checkpoint(Edit::Other);
        let mut fragment = Fragment::element(label, Payload::Reference(reference));
        fragment.text.push(' ');
        self.complete(range, fragment);
    }

    pub fn replace_image(&mut self, range: Range<usize>, mime_type: String, data: String) {
        self.checkpoint(Edit::Other);
        let mut fragment = Fragment::element("[Image]".into(), Payload::Image { mime_type, data });
        fragment.text.push(' ');
        self.complete(range, fragment);
    }

    pub fn complete_path(&mut self, range: Range<usize>, text: &str) {
        self.checkpoint(Edit::Other);
        self.complete(range, Fragment::text(text));
    }

    fn complete(&mut self, mut range: Range<usize>, mut fragment: Fragment) {
        if self.element_ending_at(range.start).is_some() {
            fragment.text.insert(0, ' ');
            for e in &mut fragment.elements {
                e.range.start += 1;
                e.range.end += 1;
            }
        }
        let suffix = &self.text()[range.end..];
        if suffix.starts_with([' ', '\t'])
            && (suffix.len() == 1 || suffix[1..].starts_with([' ', '\t']))
        {
            range.end += 1;
        }
        self.replace(range, fragment);
    }

    pub fn restore_parts(
        &mut self,
        text: &str,
        images: Vec<(String, String)>,
        resources: &[tiechui_protocol::Resource],
    ) {
        let bind = images.is_empty()
            && resources
                .iter()
                .any(|r| !r.reference.is_empty() && text.contains(&r.reference));
        let mut draft = Draft::from_parts(text, images);
        if bind {
            draft.fragment = Fragment::text(text);
            draft.cursor = text.len();
        }
        self.restore(draft);
        self.rebind_references(resources);
    }

    pub fn rebind_references(&mut self, resources: &[tiechui_protocol::Resource]) {
        let mut from = 0;
        for _ in 0..16 {
            let found = resources
                .iter()
                .filter(|r| !r.reference.is_empty())
                .filter_map(|r| {
                    self.text()[from..]
                        .find(&r.reference)
                        .map(|at| (from + at, r))
                })
                .filter(|(at, r)| {
                    !self
                        .elements()
                        .iter()
                        .any(|e| e.range.start < at + r.reference.len() && e.range.end > *at)
                })
                .min_by_key(|(at, _)| *at);
            let Some((start, r)) = found else {
                break;
            };
            let label = format!(
                "{}{}",
                if r.kind == tiechui_protocol::ResourceKind::Skill {
                    '$'
                } else {
                    '@'
                },
                r.name
            );
            from = start + label.len();
            self.replace(
                start..start + r.reference.len(),
                Fragment::element(label, Payload::Reference(r.reference.clone())),
            );
        }
        self.buffer_end();
    }

    pub fn resource_token(&self) -> Option<(Range<usize>, String, bool)> {
        if self.text().trim_start().starts_with('!') {
            return None;
        }
        let text = self.text();
        let cursor = self.draft.cursor;
        for (start, sigil) in text.char_indices().filter(|(_, c)| matches!(c, '@' | '$')) {
            if self.elements().iter().any(|e| e.range.contains(&start)) {
                continue;
            }
            if !tiechui_protocol::reference_start(text, start)
                && self.element_ending_at(start).is_none()
            {
                continue;
            }
            let before = text[..start].chars().next_back();
            let quoted = text[start + 1..].starts_with('"');
            let mut end = start + 1 + usize::from(quoted);
            while end < text.len() && self.element_starting_at(end).is_none() {
                let Some(c) = text[end..].chars().next() else {
                    break;
                };
                if c == '\n' || !quoted && c.is_whitespace() {
                    break;
                }
                end += c.len_utf8();
                if quoted
                    && c == '"'
                    && text[..end - 1]
                        .bytes()
                        .rev()
                        .take_while(|b| *b == b'\\')
                        .count()
                        % 2
                        == 0
                {
                    break;
                }
            }
            if !quoted {
                let closer = match before {
                    Some('(') => ')',
                    Some('[') => ']',
                    Some('{') => '}',
                    Some('<') => '>',
                    _ => '\0',
                };
                if text[start + 1..end].ends_with(closer) {
                    end -= closer.len_utf8();
                }
            }
            if cursor <= start || cursor > end {
                continue;
            }
            let raw = &text[start + 1..end];
            let query = if quoted {
                serde_json::from_str::<String>(raw)
                    .unwrap_or_else(|_| raw.trim_matches('"').to_owned())
            } else {
                raw.to_owned()
            };
            if sigil == '$' && tiechui_protocol::literal_dollar(&query) {
                continue;
            }
            return Some((start..end, query, sigil == '$'));
        }
        None
    }

    /// The whitespace-delimited token around the cursor, bounded by lines and elements.
    pub fn token(&self) -> Option<(Range<usize>, &str)> {
        let at = self.draft.cursor;
        let stop = |g: &str| g.chars().all(char::is_whitespace);
        let mut start = at;
        while start > 0
            && self.element_ending_at(start).is_none()
            && self.grapheme_before(start).is_some_and(|g| !stop(g))
        {
            start = self.previous(start);
        }
        let mut end = at;
        while end < self.text().len()
            && self.element_starting_at(end).is_none()
            && self.grapheme_after(end).is_some_and(|g| !stop(g))
        {
            end = self.next(end);
        }
        let inside = self
            .elements()
            .iter()
            .any(|e| e.range.start < at && at < e.range.end);
        (start < end && !inside).then(|| (start..end, &self.text()[start..end]))
    }

    pub fn replace_text(&mut self, range: Range<usize>, text: &str) {
        self.checkpoint(Edit::Other);
        self.replace(range, Fragment::text(text));
    }

    fn previous(&self, at: usize) -> usize {
        let p = self.text()[..at]
            .grapheme_indices(true)
            .next_back()
            .map_or(0, |(i, _)| i);
        self.elements()
            .iter()
            .find(|e| e.range.start < at && p < e.range.end && p >= e.range.start)
            .map_or(p, |e| e.range.start)
    }

    fn next(&self, at: usize) -> usize {
        let n = self.text()[at..]
            .graphemes(true)
            .next()
            .map_or(at, |g| at + g.len());
        self.elements()
            .iter()
            .find(|e| e.range.start <= at && at < e.range.end)
            .map_or(n, |e| e.range.end)
    }

    fn element_ending_at(&self, at: usize) -> Option<&Element> {
        self.elements().iter().find(|e| e.range.end == at)
    }

    fn element_starting_at(&self, at: usize) -> Option<&Element> {
        self.elements().iter().find(|e| e.range.start == at)
    }

    fn grapheme_before(&self, at: usize) -> Option<&str> {
        self.text()[..at].graphemes(true).next_back()
    }

    fn grapheme_after(&self, at: usize) -> Option<&str> {
        self.text()[at..].graphemes(true).next()
    }

    fn word_left(&self, mut at: usize) -> usize {
        while at > 0
            && self.element_ending_at(at).is_none()
            && self
                .grapheme_before(at)
                .is_some_and(|g| class(g) == Class::Space)
        {
            at = self.previous(at);
        }
        if let Some(element) = self.element_ending_at(at) {
            return element.range.start;
        }
        let Some(kind) = self.grapheme_before(at).map(class) else {
            return at;
        };
        while at > 0
            && self.element_ending_at(at).is_none()
            && self.grapheme_before(at).is_some_and(|g| class(g) == kind)
        {
            at = self.previous(at);
        }
        at
    }

    fn word_right(&self, mut at: usize) -> usize {
        let len = self.text().len();
        while at < len
            && self.element_starting_at(at).is_none()
            && self
                .grapheme_after(at)
                .is_some_and(|g| class(g) == Class::Space)
        {
            at = self.next(at);
        }
        if let Some(element) = self.element_starting_at(at) {
            return element.range.end;
        }
        let Some(kind) = self.grapheme_after(at).map(class) else {
            return at;
        };
        while at < len
            && self.element_starting_at(at).is_none()
            && self.grapheme_after(at).is_some_and(|g| class(g) == kind)
        {
            at = self.next(at);
        }
        at
    }

    fn line_start(&self, at: usize) -> usize {
        self.text()[..at].rfind('\n').map_or(0, |i| i + 1)
    }

    fn line_end(&self, at: usize) -> usize {
        self.text()[at..]
            .find('\n')
            .map_or(self.text().len(), |i| at + i)
    }

    fn move_to(&mut self, at: usize) {
        self.draft.cursor = at;
        self.last_edit = None;
        self.yanked = None;
        self.column = None;
    }

    pub fn buffer_start(&mut self) {
        self.move_to(0);
    }

    pub fn buffer_end(&mut self) {
        self.move_to(self.text().len());
    }

    /// Jump to the next (or previous) occurrence of a character.
    pub fn jump(&mut self, target: char, forward: bool) {
        let at = self.draft.cursor;
        let found = if forward {
            let from = self.next(at);
            self.text()[from..].find(target).map(|i| from + i)
        } else {
            self.text()[..at].rfind(target)
        };
        if let Some(found) = found {
            self.move_to(self.expand(found..found).start);
        }
    }

    pub fn left(&mut self) {
        self.move_to(self.previous(self.draft.cursor));
    }

    pub fn right(&mut self) {
        self.move_to(self.next(self.draft.cursor));
    }

    pub fn word_backward(&mut self) {
        self.move_to(self.word_left(self.draft.cursor));
    }

    pub fn word_forward(&mut self) {
        self.move_to(self.word_right(self.draft.cursor));
    }

    pub fn home(&mut self) {
        self.move_to(self.line_start(self.draft.cursor));
    }

    pub fn end(&mut self) {
        self.move_to(self.line_end(self.draft.cursor));
    }

    fn delete(&mut self, range: Range<usize>, kill: bool) {
        if range.is_empty() {
            return;
        }
        self.checkpoint(Edit::Other);
        let removed = self.replace(range, Fragment::default());
        if kill && !removed.text.is_empty() {
            self.kills.push(removed);
            if self.kills.len() > 32 {
                self.kills.remove(0);
            }
        }
    }

    pub fn backspace(&mut self) {
        let at = self.draft.cursor;
        self.delete(self.previous(at)..at, false);
    }

    pub fn delete_forward(&mut self) {
        let at = self.draft.cursor;
        self.delete(at..self.next(at), false);
    }

    pub fn delete_word_backward(&mut self) {
        let at = self.draft.cursor;
        self.delete(self.word_left(at)..at, true);
    }

    pub fn delete_word_forward(&mut self) {
        let at = self.draft.cursor;
        self.delete(at..self.word_right(at), true);
    }

    /// At a line boundary these join lines.
    pub fn delete_to_line_start(&mut self) {
        let at = self.draft.cursor;
        let start = self.line_start(at);
        self.delete(
            if start == at {
                self.previous(at)..at
            } else {
                start..at
            },
            true,
        );
    }

    pub fn delete_to_line_end(&mut self) {
        let at = self.draft.cursor;
        let end = self.line_end(at);
        self.delete(
            if end == at {
                at..self.next(at)
            } else {
                at..end
            },
            true,
        );
    }

    pub fn yank(&mut self) {
        let Some(index) = self.kills.len().checked_sub(1) else {
            return;
        };
        let start = self.draft.cursor;
        self.insert_fragment(self.kills[index].clone(), Edit::Other);
        self.yanked = Some((start..self.draft.cursor, index));
    }

    pub fn yank_pop(&mut self) {
        let Some((range, index)) = self.yanked.clone() else {
            return;
        };
        let previous = if index == 0 {
            self.kills.len() - 1
        } else {
            index - 1
        };
        self.checkpoint(Edit::Other);
        let start = range.start;
        self.replace(range, self.kills[previous].clone());
        self.yanked = Some((start..self.draft.cursor, previous));
    }

    /// Visual rows (byte ranges without the newline). Element interiors never offer breaks.
    fn layout(&self, width: usize) -> Vec<Range<usize>> {
        let width = width.max(1);
        let text = self.text();
        let inside = |at: usize| {
            self.elements()
                .iter()
                .any(|e| e.range.start < at && at < e.range.end)
        };
        let mut rows = Vec::new();
        let mut offset = 0;
        for line in text.split('\n') {
            let base = offset;
            offset += line.len() + 1;
            let mut start = base;
            let mut used = 0;
            let mut opportunity = None;
            for (index, grapheme) in line.grapheme_indices(true) {
                let at = base + index;
                let w = wrap::width(grapheme);
                if used + w > width && at > start && class(grapheme) != Class::Space {
                    let split = opportunity.filter(|&b| b > start && w < 2).unwrap_or(at);
                    rows.push(start..split);
                    used = wrap::width(&text[split..at]);
                    start = split;
                    opportunity = None;
                }
                used += w;
                let end = at + grapheme.len();
                if !inside(end) && (class(grapheme) == Class::Space || w > 1) {
                    opportunity = Some(end);
                }
            }
            rows.push(start..base + line.len());
        }
        rows
    }

    fn position(&self, rows: &[Range<usize>], at: usize) -> (usize, usize) {
        let row = rows
            .iter()
            .enumerate()
            .find(|(i, r)| {
                r.start <= at
                    && (at < r.end || at == r.end && rows.get(i + 1).is_none_or(|n| n.start > at))
            })
            .map_or(rows.len().saturating_sub(1), |(i, _)| i);
        let start = rows.get(row).map_or(0, |r| r.start);
        (row, wrap::width(&self.text()[start..at.max(start)]))
    }

    /// Every visual row `width` wide, and the cursor's row and column among them.
    pub fn rows(&self, width: usize) -> (Vec<Range<usize>>, (usize, usize)) {
        let rows = self.layout(width);
        let position = self.position(&rows, self.draft.cursor);
        (rows, position)
    }

    /// Moves between visual rows keeping the column; returns false at the first or last row.
    pub fn vertical(&mut self, width: usize, up: bool) -> bool {
        let rows = self.layout(width);
        let (row, column) = self.position(&rows, self.draft.cursor);
        let target = self.column.unwrap_or(column);
        let Some(next) = (if up {
            row.checked_sub(1)
        } else {
            Some(row + 1)
        })
        .and_then(|r| rows.get(r)) else {
            return false;
        };
        let mut at = next.start;
        for (index, grapheme) in self.text()[next.clone()].grapheme_indices(true) {
            if wrap::width(&self.text()[next.start..next.start + index + grapheme.len()]) > target {
                break;
            }
            at = next.start + index + grapheme.len();
        }
        let at = self.expand(at..at).start;
        self.draft.cursor = at;
        self.last_edit = None;
        self.yanked = None;
        self.column = Some(target);
        true
    }

    /// Places the cursor within the visible content rows, respecting atomic pastes/images.
    pub fn click(&mut self, width: usize, row: usize, column: usize) {
        let rows = self.layout(width.max(1));
        let Some(range) = rows.get(self.scroll + row) else {
            return;
        };
        let mut at = range.start;
        for (index, grapheme) in self.text()[range.clone()].grapheme_indices(true) {
            let end = range.start + index + grapheme.len();
            if wrap::width(&self.text()[range.start..end]) > column {
                break;
            }
            at = end;
        }
        self.draft.cursor = self.expand(at..at).start;
        self.column = None;
        self.last_edit = None;
        self.yanked = None;
    }

    /// Content rows after the marker, scrolled to keep the cursor visible.
    pub fn render(
        &mut self,
        width: usize,
        max_rows: usize,
        marker: Span<'static>,
        element: Style,
    ) -> Rendered {
        let indent = wrap::width(&marker.content);
        let avail = width.saturating_sub(indent).max(1);
        let mut rows = self.layout(avail);
        let (mut row, mut column) = self.position(&rows, self.draft.cursor);
        if column >= avail {
            // A full last row puts the cursor on a virtual row below it.
            row += 1;
            column = 0;
            rows.insert(row, self.draft.cursor..self.draft.cursor);
        }
        let max_rows = max_rows.max(1);
        if row < self.scroll {
            self.scroll = row;
        } else if row >= self.scroll + max_rows {
            self.scroll = row + 1 - max_rows;
        }
        self.scroll = self.scroll.min(rows.len().saturating_sub(max_rows));
        let visible = self.scroll..(self.scroll + max_rows).min(rows.len());
        let lines = rows[visible.clone()]
            .iter()
            .enumerate()
            .map(|(index, range)| {
                let prefix = if visible.start + index == 0 {
                    marker.clone()
                } else {
                    Span::raw(" ".repeat(indent))
                };
                let mut spans = vec![prefix];
                let mut at = range.start;
                for e in self
                    .elements()
                    .iter()
                    .filter(|e| e.range.start < range.end && e.range.end > range.start)
                {
                    let start = e.range.start.max(range.start);
                    if start > at {
                        spans.push(Span::raw(self.text()[at..start].to_owned()));
                    }
                    let end = e.range.end.min(range.end);
                    spans.push(Span::styled(self.text()[start..end].to_owned(), element));
                    at = end;
                }
                if at < range.end {
                    spans.push(Span::raw(self.text()[at..range.end].to_owned()));
                }
                Line::from(spans)
            })
            .collect();
        Rendered {
            lines,
            cursor: (row - visible.start, indent + column),
            above: visible.start,
            below: rows.len() - visible.end,
        }
    }

    /// Text with pastes expanded, plus images in order.
    pub fn submission(&self) -> (String, Vec<(String, String)>) {
        self.submission_parts(true)
    }

    pub fn submission_without_image_labels(&self) -> (String, Vec<(String, String)>) {
        self.submission_parts(false)
    }

    fn submission_parts(&self, image_labels: bool) -> (String, Vec<(String, String)>) {
        let mut text = String::new();
        let mut images = Vec::new();
        let mut at = 0;
        for element in self.elements() {
            text.push_str(&self.text()[at..element.range.start]);
            match &element.payload {
                Payload::Paste(content) | Payload::Reference(content) => text.push_str(content),
                Payload::Image { mime_type, data } => {
                    if image_labels {
                        text.push_str(&self.text()[element.range.clone()]);
                    }
                    images.push((mime_type.clone(), data.clone()));
                }
            }
            at = element.range.end;
        }
        text.push_str(&self.text()[at..]);
        (text, images)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(text: &str) -> Composer {
        let mut composer = Composer::default();
        composer.insert(text);
        composer
    }

    #[test]
    fn elements_are_atomic_for_motion_and_deletion() {
        let mut c = composer("a ");
        c.insert_paste(&"x".repeat(1001));
        c.insert(" b");
        assert_eq!(c.text(), "a [Pasted Content 1001 chars] b");
        c.left();
        c.left();
        c.left();
        assert_eq!(c.draft.cursor, 2);
        c.right();
        assert_eq!(c.draft.cursor, 2 + "[Pasted Content 1001 chars]".len());
        c.backspace();
        assert_eq!(c.text(), "a  b");
        c.undo();
        assert_eq!(c.submission().0, format!("a {} b", "x".repeat(1001)));
    }

    #[test]
    fn equal_pastes_are_numbered_and_images_renumber_after_delete() {
        let mut c = Composer::default();
        c.insert_paste(&"y".repeat(1200));
        c.insert_paste(&"z".repeat(1200));
        assert_eq!(
            c.text(),
            "[Pasted Content 1200 chars][Pasted Content 1200 chars #2]"
        );
        let mut c = Composer::default();
        c.insert_image("image/png".into(), "AA".into());
        c.insert(" ");
        c.insert_image("image/png".into(), "BB".into());
        assert_eq!(c.text(), "[Image #1] [Image #2]");
        c.home();
        c.delete_forward();
        assert_eq!(c.text(), " [Image #1]");
        assert_eq!(
            c.submission().1,
            [("image/png".to_owned(), "BB".to_owned())]
        );
    }

    #[test]
    fn kill_yank_and_yank_pop_keep_elements() {
        let mut c = composer("one two");
        c.delete_word_backward();
        assert_eq!(c.text(), "one ");
        c.insert_paste(&"p".repeat(1500));
        c.delete_word_backward();
        assert_eq!(c.text(), "one ");
        c.yank();
        assert!(c.text().ends_with("[Pasted Content 1500 chars]"));
        c.yank_pop();
        assert_eq!(c.text(), "one two");
        c.home();
        c.delete_to_line_end();
        assert!(c.is_empty());
    }

    #[test]
    fn word_motion_uses_character_classes() {
        let mut c = composer("foo.bar  baz");
        c.word_backward();
        assert_eq!(c.draft.cursor, 9);
        c.word_backward();
        assert_eq!(c.draft.cursor, 4);
        c.word_backward();
        assert_eq!(c.draft.cursor, 3);
        c.word_forward();
        assert_eq!(c.draft.cursor, 4);
    }

    #[test]
    fn undo_coalesces_typing_until_other_edits() {
        let mut c = Composer::default();
        for ch in ["a", "b", " ", "c"] {
            c.insert(ch);
        }
        c.undo();
        assert_eq!(c.text(), "ab ");
        c.undo();
        assert_eq!(c.text(), "ab");
        c.undo();
        assert_eq!(c.text(), "");
    }

    #[test]
    fn layout_wraps_words_cjk_and_moves_vertically() {
        let mut c = composer("hello world 中文字符");
        let rendered = c.render(10, 10, Span::raw("› "), Style::default());
        let text: Vec<String> = rendered
            .lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect();
        assert_eq!(text, ["› hello ", "  world 中", "  文字符"]);
        assert_eq!(rendered.cursor, (2, 8));
        assert!(c.vertical(8, true));
        assert_eq!(
            c.render(10, 10, Span::raw("› "), Style::default()).cursor,
            (1, 8)
        );
        c.home();
        assert!(!c.vertical(8, true));
        assert!(c.vertical(8, false));
    }

    #[test]
    fn rendering_scrolls_to_the_cursor() {
        let mut c = composer("1\n2\n3\n4\n5");
        let rendered = c.render(20, 2, Span::raw("› "), Style::default());
        assert_eq!(
            (rendered.above, rendered.below, rendered.cursor),
            (3, 0, (1, 3))
        );
    }
}
