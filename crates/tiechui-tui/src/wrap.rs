//! Grapheme-aware wrapping of styled text.

use ratatui_core::style::Style;
use ratatui_core::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

pub fn width(text: &str) -> usize {
    UnicodeWidthStr::width(text)
}

pub fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|span| width(&span.content)).sum()
}

struct Cell {
    text: String,
    style: Style,
    width: usize,
}

impl Cell {
    fn space(&self) -> bool {
        self.text.chars().all(char::is_whitespace)
    }
}

fn cells(spans: &[Span<'_>]) -> Vec<Cell> {
    spans
        .iter()
        .flat_map(|span| {
            span.content.graphemes(true).map(move |grapheme| {
                let text = if grapheme == "\t" { "    " } else { grapheme };
                Cell {
                    text: text.to_owned(),
                    style: span.style,
                    width: width(text),
                }
            })
        })
        .collect()
}

fn emit(prefix: &[Span<'static>], cells: &[Cell]) -> Line<'static> {
    let mut spans = prefix.to_vec();
    for cell in cells {
        let own = spans.len() > prefix.len();
        match spans.last_mut() {
            Some(last) if own && last.style == cell.style => {
                last.content.to_mut().push_str(&cell.text);
            }
            _ => spans.push(Span::styled(cell.text.clone(), cell.style)),
        }
    }
    Line::from(spans)
}

/// One row cut to `width`, merging equal styles; an ellipsis marks cut text when asked.
pub fn fit(spans: Vec<Span<'static>>, width: usize, ellipsis: bool) -> Line<'static> {
    let total: usize = spans.iter().map(|s| self::width(&s.content)).sum();
    let limit = if ellipsis && total > width {
        width.saturating_sub(1)
    } else {
        width
    };
    let mut used = 0;
    let mut out: Vec<Span<'static>> = Vec::new();
    'spans: for span in spans {
        for grapheme in span.content.graphemes(true) {
            let w = self::width(grapheme);
            if used + w > limit {
                break 'spans;
            }
            used += w;
            match out.last_mut() {
                Some(last) if last.style == span.style => last.content.to_mut().push_str(grapheme),
                _ => out.push(Span::styled(grapheme.to_owned(), span.style)),
            }
        }
    }
    if ellipsis && total > width && width > 0 {
        let style = out.last().map_or_else(Style::default, |s| s.style);
        out.push(Span::styled("…", style));
    }
    Line::from(out)
}

fn trim_end(mut cells: Vec<Cell>) -> Vec<Cell> {
    while cells.last().is_some_and(Cell::space) {
        cells.pop();
    }
    cells
}

/// Wraps one logical line. Prefixes count against `width`; CJK may break between characters.
pub fn wrap(
    spans: &[Span<'_>],
    width: usize,
    first: &[Span<'static>],
    rest: &[Span<'static>],
) -> Vec<Line<'static>> {
    let avail = |prefix: &[Span<'static>]| {
        width
            .saturating_sub(prefix.iter().map(|s| self::width(&s.content)).sum())
            .max(1)
    };
    let mut lines = Vec::new();
    let mut current: Vec<Cell> = Vec::new();
    let mut used = 0;
    // Index in `current` where a break may split the line.
    let mut opportunity: Option<usize> = None;
    let flush = |lines: &mut Vec<Line<'static>>, cells: Vec<Cell>| {
        let prefix = if lines.is_empty() { first } else { rest };
        lines.push(emit(prefix, &trim_end(cells)));
    };
    for cell in cells(spans) {
        let limit = avail(if lines.is_empty() { first } else { rest });
        if used + cell.width > limit && !current.is_empty() {
            if cell.space() {
                flush(&mut lines, std::mem::take(&mut current));
                used = 0;
                opportunity = None;
                continue;
            }
            match opportunity.filter(|_| cell.width < 2) {
                Some(at) if at > 0 => {
                    let mut tail = current.split_off(at);
                    flush(&mut lines, std::mem::take(&mut current));
                    let skip = tail.iter().take_while(|c| c.space()).count();
                    tail.drain(..skip);
                    used = tail.iter().map(|c| c.width).sum();
                    opportunity = tail
                        .iter()
                        .rposition(|c| c.space() || c.width > 1)
                        .map(|i| i + 1);
                    current = tail;
                }
                _ => {
                    flush(&mut lines, std::mem::take(&mut current));
                    used = 0;
                    opportunity = None;
                }
            }
        }
        if current.is_empty() && cell.space() && !lines.is_empty() {
            continue;
        }
        used += cell.width;
        let breakable = cell.space() || cell.width > 1;
        current.push(cell);
        if breakable {
            opportunity = Some(current.len());
        }
    }
    let prefix = if lines.is_empty() { first } else { rest };
    lines.push(emit(prefix, &current));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(spans: &str, width: usize) -> Vec<String> {
        wrap(
            &[Span::raw(spans)],
            width,
            &[Span::raw("• ")],
            &[Span::raw("  ")],
        )
        .iter()
        .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
        .collect()
    }

    #[test]
    fn words_break_at_spaces_and_long_words_hard_break() {
        assert_eq!(plain("alpha beta gamma", 12), ["• alpha beta", "  gamma"]);
        assert_eq!(plain("abcdefghij", 6), ["• abcd", "  efgh", "  ij"]);
    }

    #[test]
    fn cjk_and_emoji_use_display_width() {
        assert_eq!(plain("中文字符测试", 8), ["• 中文字", "  符测试"]);
        assert_eq!(plain("ok 👍🏽 done", 7), ["• ok 👍🏽", "  done"]);
        assert_eq!(
            plain("e\u{301}e\u{301}e\u{301}", 4),
            ["• e\u{301}e\u{301}", "  e\u{301}"]
        );
    }

    #[test]
    fn fit_cuts_by_display_width_with_optional_ellipsis() {
        let line = |l: Line<'_>| {
            l.spans
                .iter()
                .map(|s| s.content.to_string())
                .collect::<String>()
        };
        assert_eq!(line(fit(vec![Span::raw("中文字符")], 5, false)), "中文");
        assert_eq!(line(fit(vec![Span::raw("abcdef")], 4, true)), "abc…");
        assert_eq!(line(fit(vec![Span::raw("abc")], 4, true)), "abc");
    }

    #[test]
    fn empty_line_keeps_prefix_and_tabs_expand() {
        assert_eq!(plain("", 10), ["• "]);
        assert_eq!(plain("\tx", 10), ["•     x"]);
    }
}
