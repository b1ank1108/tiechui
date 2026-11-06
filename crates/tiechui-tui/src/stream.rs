//! Streaming assistant text: newline-complete lines go to scrollback, the rest stays live.

use ratatui_core::text::Line;

use crate::cells;
use crate::theme::Theme;
use crate::wrap;

#[derive(Debug, Default)]
pub struct Stream {
    source: String,
    committed_source: usize,
    committed_lines: usize,
}

fn render(source: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = cells::assistant(source, width, theme);
    while lines.last().is_some_and(|line| wrap::line_width(line) == 0) {
        lines.pop();
    }
    lines
}

/// An open code fence would render its closing border early, and a `|` line may still become
/// a table, so both are held back until the block is complete.
fn stable_end(source: &str, end: usize) -> usize {
    let mut open_fence = None;
    let mut table_start = None;
    let mut offset = 0;
    for line in source[..end].split_inclusive('\n') {
        let trimmed = line.trim_start();
        let indent = line.len() - trimmed.len();
        if indent <= 3 && (trimmed.starts_with("```") || trimmed.starts_with("~~~")) {
            open_fence = if open_fence.is_some() {
                None
            } else {
                Some(offset)
            };
        }
        if open_fence.is_none() {
            if trimmed.starts_with('|') {
                table_start.get_or_insert(offset);
            } else {
                table_start = None;
            }
        }
        offset += line.len();
    }
    open_fence.or(table_start).unwrap_or(end)
}

impl Stream {
    pub fn push(&mut self, delta: &str) {
        self.source.push_str(delta);
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Source newlines are line breaks, so lines before the last newline no longer change.
    pub fn commit(&mut self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let Some(end) = self
            .source
            .rfind('\n')
            .map(|i| i + 1)
            .map(|end| stable_end(&self.source, end))
        else {
            return Vec::new();
        };
        if end <= self.committed_source {
            return Vec::new();
        }
        let rendered = render(&self.source[..end], width, theme);
        let out = rendered
            .get(self.committed_lines.min(rendered.len())..)
            .map(<[_]>::to_vec)
            .unwrap_or_default();
        self.committed_source = end;
        self.committed_lines = self.committed_lines.max(rendered.len());
        out
    }

    pub fn tail(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let rendered = render(&self.source, width, theme);
        rendered
            .get(self.committed_lines.min(rendered.len())..)
            .map(<[_]>::to_vec)
            .unwrap_or_default()
    }

    /// The completed item text is authoritative; only lines not yet in scrollback are returned.
    pub fn finish(self, text: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        let rendered = render(text, width, theme);
        rendered
            .get(self.committed_lines.min(rendered.len())..)
            .map(<[_]>::to_vec)
            .unwrap_or_default()
    }

    pub fn committed(&self) -> bool {
        self.committed_lines > 0
    }

    pub fn history(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        if self.committed_source == 0 {
            return Vec::new();
        }
        render(&self.source[..self.committed_source], width, theme)
    }

    /// Update the committed boundary before replaying history at the new width.
    pub fn resize(&mut self, width: usize, theme: &Theme) {
        if self.committed_source > 0 {
            self.committed_lines =
                render(&self.source[..self.committed_source], width, theme).len();
        }
    }
}
