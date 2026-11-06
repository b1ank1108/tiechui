//! History cells: finished transcript entries rendered into native scrollback.

use std::time::Duration;

use ratatui_core::style::{Modifier, Style};
use ratatui_core::text::{Line, Span};

use crate::activity;
use crate::markdown;
use crate::theme::Theme;
use crate::tools::{self, Tool, View};
use crate::wrap::{self, wrap};

#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    /// The session header, printed when a session opens.
    Header(crate::header::Header),
    Compaction {
        summary: String,
        tokens_before: u64,
        degraded: bool,
    },
    User(String),
    InlineUser(String),
    Assistant(String),
    Thinking(String),
    ThinkingSummary(String),
    /// One group from `tools::groups`.
    Tools(Vec<Tool>),
    /// Between tool activity and the answer that follows it.
    Separator,
    Worked(Duration),
    Error(String),
    /// A recoverable state worth noticing, such as paused automatic compaction.
    Warning(String),
    Interrupted,
    /// A command's report: its title, then plain text.
    Info {
        title: String,
        text: String,
    },
}

pub const INTERRUPTED: &str = "Conversation interrupted - tell the model what to do differently.";

/// Columns before a control row's label.
const CONTROL_INDENT: usize = 4;
const CONTROL_PADDING: usize = 2;

/// What a folded cell hides, which names its control row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Fold {
    /// Output rows cut from a preview; its `+ N lines` row is the control.
    Lines,
    Details,
    Summary,
    Thinking,
}

impl Cell {
    /// Scrollback rendering.
    pub fn lines(&self, width: usize, theme: &Theme) -> Vec<Line<'static>> {
        self.render(width, theme, View::History, false)
    }

    /// The form in app-drawn transcripts, where a click can expand a cell in place: foldable
    /// cells end in a dim control row, the activity disclosure, whose index is returned.
    pub fn folded(
        &self,
        width: usize,
        theme: &Theme,
        unfolded: bool,
        show_thinking: bool,
    ) -> (Vec<Line<'static>>, Option<usize>) {
        let fold = match self {
            Cell::Tools(tools) => tools::fold(tools, width, theme),
            Cell::Compaction { summary, .. } if !summary.trim().is_empty() => Some(Fold::Summary),
            Cell::ThinkingSummary(_) => Some(Fold::Thinking),
            _ => None,
        };
        let Some(fold) = fold else {
            return (
                self.render(width, theme, View::History, show_thinking),
                None,
            );
        };
        let mut lines = match self {
            _ if unfolded => self.render(width, theme, View::Transcript, true),
            Cell::Compaction {
                summary,
                tokens_before,
                degraded,
            } => checkpoint(
                summary,
                *tokens_before,
                *degraded,
                false,
                false,
                width,
                theme,
            ),
            _ => self.render(width, theme, View::History, show_thinking),
        };
        if lines.is_empty() {
            return (lines, None);
        }
        let label = match (fold, unfolded) {
            (Fold::Lines, false) => {
                let row = lines.len() - 1;
                return (lines, Some(row));
            }
            (Fold::Lines, true) => "− Show less",
            (Fold::Details, false) => "+ Show details",
            (Fold::Details, true) => "− Hide details",
            (Fold::Summary, false) => "+ Show summary",
            (Fold::Summary, true) => "− Hide summary",
            (Fold::Thinking, false) => "+ Show thinking",
            (Fold::Thinking, true) => "− Hide thinking",
        };
        lines.push(wrap::fit(
            vec![
                Span::raw(" ".repeat(CONTROL_INDENT)),
                Span::styled(label, theme.dim),
            ],
            width,
            true,
        ));
        let row = lines.len() - 1;
        (lines, Some(row))
    }

    pub fn render(
        &self,
        width: usize,
        theme: &Theme,
        view: View,
        show_thinking: bool,
    ) -> Vec<Line<'static>> {
        match self {
            Cell::Header(_) if matches!(view, View::Transcript) => Vec::new(),
            Cell::Header(header) => crate::header::render(header, width, theme),
            Cell::Compaction {
                summary,
                tokens_before,
                degraded,
            } => {
                let expanded = matches!(view, View::Transcript);
                checkpoint(
                    summary,
                    *tokens_before,
                    *degraded,
                    expanded,
                    !expanded,
                    width,
                    theme,
                )
            }
            Cell::User(text) => user(text, width, theme, false),
            Cell::InlineUser(text) => user(&tools::plain(text), width, theme, true),
            Cell::Assistant(source) => assistant(source, width, theme),
            Cell::ThinkingSummary(text) if matches!(view, View::Transcript) => {
                thinking(text, width, theme)
            }
            Cell::ThinkingSummary(text) => {
                let Some(title) = activity::heading(text) else {
                    return Vec::new();
                };
                wrap(
                    &[Span::styled(
                        format!("Thought · {}", tools::plain(title)),
                        theme.dim,
                    )],
                    width,
                    &[Span::styled("• ", theme.dim)],
                    &[Span::raw("  ")],
                )
            }
            Cell::Thinking(text) => {
                let full = matches!(view, View::Transcript);
                let (content, visible) = activity::reasoning(text);
                match (full, visible, show_thinking) {
                    (true, ..) => thinking(text, width, theme),
                    (false, true, _) | (false, false, true) => thinking(&content, width, theme),
                    _ => Vec::new(),
                }
            }
            Cell::Tools(tools) => tools::render(tools, width, theme, view),
            Cell::Separator => vec![Line::from(Span::styled("─".repeat(width), theme.dim))],
            Cell::Worked(duration) => {
                let label = format!(" Worked for {} ", activity::elapsed(*duration));
                let rule = width.saturating_sub(wrap::width(&label));
                let left = rule / 2;
                vec![wrap::fit(
                    vec![Span::styled(
                        format!("{}{label}{}", "─".repeat(left), "─".repeat(rule - left)),
                        theme.dim,
                    )],
                    width,
                    false,
                )]
            }
            Cell::Error(message) => banner("■ ", message, theme.error, width),
            Cell::Warning(message) => banner("▲ ", message, theme.warning, width),
            Cell::Interrupted => banner("■ ", INTERRUPTED, theme.error, width),
            Cell::Info { title, text } if title == "/goal" => {
                crate::goal::report(text, width, theme)
            }
            Cell::Info { title, text } => {
                let mut lines = wrap(
                    &[Span::styled(
                        title.clone(),
                        Style::default().add_modifier(Modifier::BOLD),
                    )],
                    width,
                    &[Span::styled("• ", theme.dim)],
                    &[Span::raw("  ")],
                );
                for row in text.lines() {
                    lines.extend(wrap(
                        &[Span::styled(row.to_owned(), theme.dim)],
                        width,
                        &[Span::raw("  ")],
                        &[Span::raw("  ")],
                    ));
                }
                lines
            }
        }
    }
}

/// The label and two columns on either side share one hover and click target.
pub fn on_control(line: &Line<'_>, column: usize, width: usize) -> bool {
    let end = wrap::line_width(line).min(width);
    end > CONTROL_INDENT
        && (CONTROL_INDENT.saturating_sub(CONTROL_PADDING)
            ..end.saturating_add(CONTROL_PADDING).min(width))
            .contains(&column)
}

/// A control row under the pointer: its label in Accent and bold, like a focused link.
pub fn hovered(line: &Line<'_>, theme: &Theme) -> Line<'static> {
    let label: String = line
        .spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect();
    Line::from(vec![
        Span::raw(" ".repeat(CONTROL_INDENT)),
        Span::styled(
            label.trim_start().to_owned(),
            theme.accent.add_modifier(Modifier::BOLD),
        ),
    ])
}

fn pad(mut line: Line<'static>, width: usize, style: Style) -> Line<'static> {
    let used = wrap::line_width(&line);
    if used < width {
        line.spans.push(Span::raw(" ".repeat(width - used)));
    }
    for span in &mut line.spans {
        span.style = style.patch(span.style);
    }
    line
}

/// User text on the main soft surface or in the compact inline presentation.
fn user(text: &str, width: usize, theme: &Theme, inline: bool) -> Vec<Line<'static>> {
    let surface = theme.user();
    let text_style = if inline {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default()
    };
    let marker = Span::styled(
        "› ",
        Style::default().add_modifier(Modifier::BOLD | Modifier::DIM),
    );
    let mut lines = if inline {
        Vec::new()
    } else {
        vec![pad(Line::default(), width, surface)]
    };
    for (index, part) in text.split('\n').enumerate() {
        let first = if index == 0 {
            marker.clone()
        } else {
            Span::raw("  ")
        };
        for line in wrap(
            &[Span::styled(part.to_owned(), text_style)],
            width,
            &[first],
            &[Span::raw("  ")],
        ) {
            lines.push(if inline {
                line
            } else {
                pad(line, width, surface)
            });
        }
    }
    if !inline {
        lines.push(pad(Line::default(), width, surface));
    }
    lines
}

/// Markdown under a dim `•` with a fixed two-column indent.
pub fn assistant(source: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let mut lines = markdown::render(source, width.saturating_sub(2).max(1), theme);
    while lines.last().is_some_and(|line| wrap::line_width(line) == 0) {
        lines.pop();
    }
    lines
        .into_iter()
        .enumerate()
        .map(|(index, mut line)| {
            let prefix = if index == 0 {
                Span::styled("• ", theme.dim)
            } else if line.spans.is_empty() {
                return line;
            } else {
                Span::raw("  ")
            };
            line.spans.insert(0, prefix);
            line
        })
        .collect()
}

/// Thinking under a dim `•`, dim and italic over the Markdown styles.
fn thinking(source: &str, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let base = theme.dim.add_modifier(Modifier::ITALIC);
    let mut lines = assistant(source, width, theme);
    for line in &mut lines {
        for (index, span) in line.spans.iter_mut().enumerate() {
            if index > 0 {
                span.style = base.patch(span.style);
            }
        }
    }
    if lines.iter().all(|line| wrap::line_width(line) <= 2) {
        return Vec::new();
    }
    lines
}

fn banner(marker: &'static str, message: &str, style: Style, width: usize) -> Vec<Line<'static>> {
    wrap(
        &[Span::styled(message.to_owned(), style)],
        width,
        &[Span::styled(marker, style)],
        &[Span::raw("  ")],
    )
}

/// A context checkpoint: one `◇` line, `◆` with a warning when the summary is partial; the
/// transcript adds the summary itself.
fn checkpoint(
    summary: &str,
    tokens_before: u64,
    degraded: bool,
    expanded: bool,
    hint: bool,
    width: usize,
    theme: &Theme,
) -> Vec<Line<'static>> {
    let mut title = vec![
        if degraded {
            Span::styled("◆ ", theme.warning)
        } else {
            Span::styled("◇ ", theme.dim)
        },
        Span::styled(
            "Compacted context",
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!(
                " · {} tokens summarized",
                crate::status::compact_tokens(tokens_before)
            ),
            theme.dim,
        ),
    ];
    if degraded {
        title.push(Span::styled(" · ", theme.dim));
        title.push(Span::styled("partial", theme.warning));
    }
    if hint {
        title.push(Span::styled(" · ctrl+o to view", theme.dim));
    }
    let mut lines = wrap(&title, width, &[], &[Span::raw("  ")]);
    if degraded {
        lines.extend(wrap(
            &[Span::styled(
                "Earlier messages did not fit the summary request.",
                theme.dim,
            )],
            width,
            &[Span::styled("  └ ", theme.dim)],
            &[Span::raw("    ")],
        ));
    }
    if expanded {
        lines.push(Line::default());
        for mut line in markdown::render(summary, width.saturating_sub(2).max(1), theme) {
            if !line.spans.is_empty() {
                line.spans.insert(0, Span::raw("  "));
            }
            lines.push(line);
        }
        while lines.last().is_some_and(|line| wrap::line_width(line) == 0) {
            lines.pop();
        }
    }
    lines
}
