//! Tool activity cells: Explored and Maintained context groups, Ran / Updated / Called,
//! previews and running state.

use std::ops::Range;
use std::time::{Duration, Instant};

use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::{Line, Span};
use serde_json::Value;

use crate::activity;
use crate::cells::Fold;
use crate::diff;
use crate::status::compact_tokens;
use crate::theme::Theme;
use crate::wrap::{self, wrap};

const PREVIEW_ROWS: usize = 3;
const DIFF_PREVIEW_ROWS: usize = 8;
/// The compaction module's history tools and their step labels.
const MAINTENANCE: [(&str, &str); 4] = [
    ("context_status", "Status"),
    ("search_history", "Search"),
    ("read_history", "Read"),
    ("compact_history", "Compact"),
];

#[derive(Debug, Clone, PartialEq)]
pub struct Tool {
    pub id: String,
    pub name: String,
    pub arguments: Value,
    /// The final `ToolOutput` JSON.
    pub output: Option<Value>,
    /// The newest streamed `ToolOutput` while running.
    pub partial: Option<Value>,
    pub error: bool,
    pub started: Option<Instant>,
    pub duration: Option<Duration>,
}

impl Tool {
    pub fn new(id: String, name: String, arguments: Value) -> Self {
        Self {
            id,
            name,
            arguments,
            output: None,
            partial: None,
            error: false,
            started: None,
            duration: None,
        }
    }

    pub fn done(&self) -> bool {
        self.output.is_some()
    }
}

/// How a cell is shown: live with animation, in scrollback, or expanded in the transcript.
#[derive(Debug, Clone, Copy)]
pub enum View {
    Live { phase: Duration, now: Instant },
    History,
    Transcript,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    Read,
    Search,
    List,
    Ran,
    /// A background shell task: started, read, stopped or listed.
    Shell,
    Updated,
    Called,
}

impl Action {
    fn of(tool: &Tool) -> Self {
        match tool.name.as_str() {
            "read" => Self::Read,
            "grep" | "find" => Self::Search,
            "ls" => Self::List,
            // A background command that ended at once reports like a foreground one.
            "bash"
                if shell(tool).is_some()
                    || (!tool.done() && tool.arguments["background"] == true) =>
            {
                Self::Shell
            }
            "bash" => Self::Ran,
            "shell_output" | "shell_stop" | "shell_list" => Self::Shell,
            "edit" | "write" => Self::Updated,
            _ => Self::Called,
        }
    }

    fn label(self, tool: &Tool, live: bool) -> &'static str {
        match self {
            Self::Read => "Read",
            Self::Search => "Search",
            Self::List => "List",
            Self::Ran => "Ran",
            Self::Shell => match (tool.name.as_str(), live) {
                ("bash", true) => "Starting",
                ("bash", false) => "Started",
                ("shell_stop", true) => "Stopping",
                ("shell_stop", false) => "Stopped",
                ("shell_list", true) => "Listing",
                ("shell_list", false) => "Listed",
                (_, true) => "Waiting",
                _ if tool
                    .output
                    .as_ref()
                    .is_some_and(|o| !shell_text(o).is_empty()) =>
                {
                    "Output"
                }
                _ => "Waited",
            },
            Self::Updated => "Updated",
            Self::Called => "Called",
        }
    }
}

fn exploration(tool: &Tool) -> bool {
    matches!(
        Action::of(tool),
        Action::Read | Action::Search | Action::List
    ) && !(tool.done() && tool.error)
}

/// The task a shell tool reports on, as the exec module records it in `details`.
fn shell(tool: &Tool) -> Option<&Value> {
    tool.output
        .as_ref()
        .map(|output| &output["details"]["shell"])
        .filter(|shell| shell.is_object())
}

fn task_id(tool: &Tool) -> Option<&str> {
    shell(tool)
        .and_then(|shell| shell["task_id"].as_str())
        .or_else(|| argument(tool, &["task_id"]))
}

/// The output a shell tool returned, without the bracketed status line kept for the model.
fn shell_text(output: &Value) -> String {
    if let Some(text) = output["structured_content"]["output"].as_str() {
        return plain(text).trim_end().to_owned();
    }
    if output["details"]["shell"]["new_output"] == false {
        return String::new();
    }
    let text = output_text(output);
    let body = match text.rsplit_once("\n\n[") {
        Some((body, note)) if note.ends_with(']') => body.trim_end(),
        _ => &text,
    };
    match body {
        "(no new output)" | "(no output yet)" => String::new(),
        body => body.to_owned(),
    }
}

/// `exit 0`, or the task's state while it has no exit code.
fn shell_state(tool: &Tool) -> Option<String> {
    let shell = shell(tool)?;
    if let Some(code) = shell["exit_code"].as_i64() {
        return Some(format!("exit {code}"));
    }
    // A start says it is running.
    let state = shell["state"].as_str().filter(|_| tool.name != "bash")?;
    Some(state.replace('_', " "))
}

/// A `shell_output` read's task, so consecutive reads of one task share a group.
fn poll(tool: &Tool) -> Option<&str> {
    if tool.name != "shell_output" || (tool.done() && tool.error) {
        return None;
    }
    task_id(tool)
}

fn maintenance(tool: &Tool) -> Option<&'static str> {
    MAINTENANCE
        .iter()
        .find(|(name, _)| *name == tool.name)
        .map(|(_, label)| *label)
}

/// Consecutive explorations share a group, as do consecutive history tools and consecutive
/// reads of one shell task; everything else stands alone, in call order.
pub fn groups(tools: &[Tool]) -> Vec<Range<usize>> {
    let mut groups: Vec<Range<usize>> = Vec::new();
    for (index, tool) in tools.iter().enumerate() {
        let first = groups.last().map(|last| &tools[last.start]);
        match groups.last_mut() {
            Some(last)
                if first.is_some_and(|first| {
                    (exploration(tool) && exploration(first))
                        || (maintenance(tool).is_some() && maintenance(first).is_some())
                        || poll(tool).is_some_and(|id| poll(first) == Some(id))
                }) =>
            {
                last.end = index + 1
            }
            _ => groups.push(index..index + 1),
        }
    }
    groups
}

/// Removes terminal control sequences so tool output cannot move the cursor or recolor.
pub fn plain(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\x1b' => {
                if chars.next_if_eq(&'[').is_some() {
                    while chars.next().is_some_and(|c| !('@'..='~').contains(&c)) {}
                } else if chars.next_if_eq(&']').is_some() {
                    while let Some(c) = chars.next() {
                        if c == '\x07' || (c == '\x1b' && chars.next_if_eq(&'\\').is_some()) {
                            break;
                        }
                    }
                } else {
                    chars.next();
                }
            }
            '\t' => out.push_str("    "),
            '\r' => {}
            c if c == '\n' || !c.is_control() => out.push(c),
            _ => {}
        }
    }
    out
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

fn argument<'a>(tool: &'a Tool, keys: &[&str]) -> Option<&'a str> {
    keys.iter()
        .find_map(|key| tool.arguments[*key].as_str())
        .filter(|value| !value.is_empty())
}

fn command(tool: &Tool) -> String {
    plain(argument(tool, &["command", "code"]).unwrap_or(&tool.name))
}

fn subject(tool: &Tool) -> String {
    let value = match Action::of(tool) {
        Action::Called => return single_line(&tool.name),
        Action::Ran => Some(command(tool)),
        Action::Shell if tool.name == "shell_list" => Some("shells".into()),
        Action::Shell => {
            let command = shell(tool)
                .and_then(|shell| shell["command"].as_str())
                .or_else(|| argument(tool, &["command"]))
                .map(|command| {
                    let command = plain(command);
                    let mut first = command.lines().next().unwrap_or_default().to_owned();
                    if command.trim_end().contains('\n') {
                        first.push_str(" …");
                    }
                    first
                });
            let parts: Vec<String> = [task_id(tool).map(str::to_owned), command]
                .into_iter()
                .flatten()
                .collect();
            (!parts.is_empty()).then(|| parts.join(" · "))
        }
        Action::Search => {
            let parts: Vec<&str> = ["pattern", "path"]
                .iter()
                .filter_map(|key| argument(tool, &[key]))
                .collect();
            (!parts.is_empty()).then(|| parts.join(" · "))
        }
        _ => argument(tool, &["path", "file_path"]).map(str::to_owned),
    };
    single_line(&value.unwrap_or_else(|| tool.name.clone()))
}

pub(crate) fn summary(tool: &Tool) -> String {
    format!(
        "{} {}",
        Action::of(tool).label(tool, !tool.done()),
        subject(tool)
    )
}

/// Text parts of a `ToolOutput`, without trailing blank lines.
pub fn output_text(output: &Value) -> String {
    let text = output["content"]
        .as_array()
        .map(|parts| {
            parts
                .iter()
                .filter_map(|part| match part["kind"].as_str() {
                    Some("text") => part["text"].as_str().map(plain),
                    Some("image") => Some("[image]".to_owned()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default();
    text.trim_end().to_owned()
}

fn running_bullet(tool: &Tool, theme: &Theme, phase: Duration, now: Instant) -> Span<'static> {
    let elapsed = tool.started.map_or(Duration::ZERO, |started| now - started);
    activity::bullet(theme, phase, elapsed)
}

/// Explored: one header and merged steps (`Read a, b`), or every tool when expanded.
fn explored(tools: &[Tool], width: usize, theme: &Theme, view: View) -> Vec<Line<'static>> {
    let live = match view {
        View::Live { phase, now } => tools.iter().find(|t| !t.done()).map(|t| (t, phase, now)),
        _ => None,
    };
    let header = match live {
        Some((tool, phase, now)) => vec![
            running_bullet(tool, theme, phase, now),
            Span::raw(" "),
            Span::styled("Exploring", Style::default().add_modifier(Modifier::BOLD)),
        ],
        None => vec![
            Span::styled("• ", theme.dim),
            Span::styled("Explored", Style::default().add_modifier(Modifier::BOLD)),
        ],
    };
    let mut lines = vec![wrap::fit(header, width, true)];
    if matches!(view, View::Transcript) {
        for tool in tools {
            lines.extend(single(tool, width, theme, view, false));
        }
        return lines;
    }
    let mut steps: Vec<(Action, Vec<&Tool>)> = Vec::new();
    for tool in tools {
        let action = Action::of(tool);
        match steps.last_mut() {
            Some((previous, step)) if *previous == action => {
                if !step.iter().any(|seen| subject(seen) == subject(tool)) {
                    step.push(tool);
                }
            }
            _ => steps.push((action, vec![tool])),
        }
    }
    let mut rows = Vec::new();
    for (index, (action, step)) in steps.into_iter().enumerate() {
        let lead = if index == 0 { "  └ " } else { "    " };
        let label = action.label(step[0], false);
        let mut content = Vec::new();
        let paths: Vec<String> = step
            .iter()
            .map(|tool| {
                let key: &[&str] = match action {
                    Action::Search => &["path"],
                    _ => &["path", "file_path"],
                };
                argument(tool, key).map(single_line).unwrap_or_default()
            })
            .collect();
        for (position, (tool, path)) in step.iter().zip(short_names(&paths)).enumerate() {
            if position > 0 {
                content.push(Span::styled(", ", theme.dim));
            }
            match action {
                Action::Search => {
                    let pattern = argument(tool, &["pattern"]).map(single_line);
                    content.push(Span::raw(
                        pattern.unwrap_or_else(|| single_line(&tool.name)),
                    ));
                    if !path.is_empty() {
                        content.push(Span::styled(" in ", theme.dim));
                        content.push(Span::raw(path));
                    }
                }
                _ if path.is_empty() => content.push(Span::raw(single_line(&tool.name))),
                _ => content.push(Span::raw(path)),
            }
        }
        // Continuation rows hang under the first subject, past the step's label.
        rows.extend(wrap(
            &content,
            width,
            &[
                Span::styled(lead, theme.dim),
                Span::styled(label, Style::default().fg(Color::Cyan)),
                Span::raw(" "),
            ],
            &[Span::raw(" ".repeat(wrap::width(lead) + label.len() + 1))],
        ));
    }
    if rows.len() > PREVIEW_ROWS {
        // The rest is under Show details.
        rows.truncate(PREVIEW_ROWS);
        if let Some(last) = rows.pop() {
            let mut spans = last.spans;
            spans.push(Span::styled(" …", theme.dim));
            rows.push(wrap::fit(spans, width, true));
        }
    }
    lines.extend(rows);
    lines
}

/// The last component of each path, with as many parents as it takes to tell equal names apart.
fn short_names(paths: &[String]) -> Vec<String> {
    let parts: Vec<Vec<&str>> = paths
        .iter()
        .map(|path| {
            path.split(['/', '\\'])
                .filter(|part| !part.is_empty())
                .collect()
        })
        .collect();
    let mut depth = vec![1; paths.len()];
    loop {
        let names: Vec<String> = parts
            .iter()
            .zip(paths)
            .zip(&depth)
            .map(|((parts, path), depth)| match parts.len() {
                0 => path.clone(),
                count => parts[count - (*depth).min(count)..].join("/"),
            })
            .collect();
        let mut longer = false;
        for index in 0..paths.len() {
            let clash = (0..paths.len())
                .any(|other| names[other] == names[index] && paths[other] != paths[index]);
            if clash && depth[index] < parts[index].len() {
                depth[index] += 1;
                longer = true;
            }
        }
        if !longer {
            return names;
        }
    }
}

/// A history tool's result: its JSON, or the raw text when it is not JSON.
fn receipt(tool: &Tool) -> Option<Value> {
    let text = output_text(tool.output.as_ref()?);
    Some(serde_json::from_str(&text).unwrap_or(Value::String(text)))
}

/// Cooldown refusals are expected and not failures.
fn deferred(result: &Value) -> bool {
    result["error"]["code"] == "MAINTENANCE_DEFERRED"
}

fn count(n: usize, noun: &str) -> String {
    format!("{n} {noun}{}", if n == 1 { "" } else { "s" })
}

/// What a history call asked for, then what it found.
fn maintenance_step(tool: &Tool, result: Option<&Value>) -> String {
    let ok = result.filter(|result| result["ok"] == true);
    let found = |key: &str| ok.and_then(|result| result[key].as_array()).map(Vec::len);
    let parts: Vec<String> = match tool.name.as_str() {
        "context_status" => ok.map_or_else(Vec::new, |result| {
            let budget = &result["budget"];
            let mut parts = vec![format!(
                "{} / {}",
                compact_tokens(budget["input_tokens_estimate"].as_u64().unwrap_or(0)),
                compact_tokens(budget["model_window_tokens"].as_u64().unwrap_or(0))
            )];
            parts.extend(budget["pressure"].as_str().map(str::to_owned));
            parts.extend(
                found("candidates")
                    .filter(|n| *n > 0)
                    .map(|n| count(n, "candidate")),
            );
            parts.extend(
                result["maintenance"]["state"]
                    .as_str()
                    .filter(|state| matches!(*state, "cooldown" | "paused"))
                    .map(str::to_owned),
            );
            parts
        }),
        "search_history" => [
            argument(tool, &["query"]).map(|query| format!("\"{query}\"")),
            found("results").map(|n| count(n, "hit")),
        ]
        .into_iter()
        .flatten()
        .collect(),
        "read_history" => [
            argument(tool, &["ref"]).map(str::to_owned),
            found("records").map(|n| count(n, "record")),
        ]
        .into_iter()
        .flatten()
        .collect(),
        _ => {
            let blocks = ok.and_then(|result| result["blocks"].as_array());
            let ranges = tool.arguments["ranges"].as_array().into_iter().flatten();
            let titles: Vec<String> = ranges
                .map(|range| {
                    let id = range["candidate_id"].as_str().unwrap_or_default();
                    let title = range["title"].as_str().filter(|t| !t.is_empty());
                    let block = blocks
                        .and_then(|blocks| blocks.iter().find(|b| b["candidate_id"] == id))
                        .and_then(|block| block["ref"].as_str());
                    match (block, title) {
                        (Some(block), Some(title)) => format!("{block} {title}"),
                        (Some(block), None) => block.to_owned(),
                        (None, title) => title.unwrap_or(id).to_owned(),
                    }
                })
                .collect();
            vec![titles.join(", ")]
        }
    };
    let parts: Vec<String> = parts.into_iter().filter(|part| !part.is_empty()).collect();
    single_line(&plain(&parts.join(" · ")))
}

/// Maintained context: consecutive history tools under one `◇` header, a row per call;
/// arguments and summaries only in the transcript.
fn maintained(tools: &[Tool], width: usize, theme: &Theme, view: View) -> Vec<Line<'static>> {
    let results: Vec<Option<Value>> = tools.iter().map(receipt).collect();
    let failed = tools
        .iter()
        .zip(&results)
        .any(|(tool, result)| tool.error && result.as_ref().is_some_and(|r| !deferred(r)));
    let saved: u64 = results
        .iter()
        .flatten()
        .filter_map(|result| result["effect"]["net_saved_tokens_estimate"].as_u64())
        .sum();
    let bold = Style::default().add_modifier(Modifier::BOLD);
    let running = match view {
        View::Live { phase, now } => tools.iter().find(|t| !t.done()).map(|t| (t, phase, now)),
        _ => None,
    };
    let mut header = match running {
        Some((tool, phase, now)) => vec![
            activity::diamond(
                theme,
                phase,
                tool.started.map_or(Duration::ZERO, |started| now - started),
            ),
            Span::raw(" "),
            Span::styled("Maintaining context", bold),
        ],
        None if failed => vec![
            Span::styled("◇ ", theme.error.add_modifier(Modifier::BOLD)),
            Span::styled("Maintained context", bold),
        ],
        None => vec![
            Span::styled("◇ ", theme.dim),
            Span::styled("Maintained context", bold),
        ],
    };
    if saved > 0 {
        header.push(Span::styled(
            format!(" · ≈ −{} tokens", compact_tokens(saved)),
            theme.dim,
        ));
    }
    let mut lines = vec![wrap::fit(header, width, true)];
    if matches!(view, View::Transcript) {
        for tool in tools {
            lines.extend(single(tool, width, theme, view, false));
        }
        return lines;
    }
    for (index, (tool, result)) in tools.iter().zip(&results).enumerate() {
        let mut spans = vec![
            tree(index, theme),
            Span::styled(
                maintenance(tool).unwrap_or_default(),
                Style::default().fg(Color::Cyan),
            ),
        ];
        let step = maintenance_step(tool, result.as_ref());
        let mut parts = Vec::new();
        if !step.is_empty() {
            parts.push(Span::raw(step));
        }
        match result {
            None if running.is_some() => parts.push(Span::styled(
                if tool.started.is_some() {
                    "running"
                } else {
                    "pending"
                },
                theme.dim,
            )),
            Some(result) if deferred(result) => {
                parts.push(Span::styled("deferred · cooldown", theme.dim))
            }
            Some(result) if tool.error => match result["error"]["code"].as_str() {
                Some(code) => {
                    parts.push(Span::styled(code.to_owned(), theme.error));
                    parts.extend(
                        result["error"]["message"]
                            .as_str()
                            .map(|message| Span::styled(single_line(&plain(message)), theme.dim)),
                    );
                }
                None => parts.push(Span::styled(
                    single_line(&plain(result.as_str().unwrap_or("failed"))),
                    theme.error,
                )),
            },
            _ => {}
        }
        for (index, part) in parts.into_iter().enumerate() {
            spans.push(if index == 0 {
                Span::raw(" ")
            } else {
                Span::styled(" · ", theme.dim)
            });
            spans.push(part);
        }
        lines.push(wrap::fit(spans, width, true));
    }
    lines
}

fn single(
    tool: &Tool,
    width: usize,
    theme: &Theme,
    view: View,
    bullet: bool,
) -> Vec<Line<'static>> {
    let kind = Action::of(tool);
    let expanded = matches!(view, View::Transcript);
    let live = match view {
        View::Live { phase, now } if !tool.done() => Some((phase, now)),
        _ => None,
    };
    let failed = tool.done() && tool.error;
    let action = match kind {
        Action::Called if live.is_some() => "Calling",
        Action::Ran if live.is_some() && tool.started.is_some() => "Running",
        Action::Ran if live.is_some() => "Run",
        Action::Ran | Action::Called => kind.label(tool, false),
        _ if failed => "Failed",
        _ => kind.label(tool, live.is_some()),
    };
    // A starting command's title already says it is running.
    let starting = kind == Action::Shell && tool.name == "bash";
    let mut suffix = String::new();
    if live.is_some() && kind != Action::Ran && !starting {
        suffix.push_str(if tool.started.is_some() {
            " · running"
        } else {
            " · pending"
        });
    }
    if kind == Action::Shell
        && !failed
        && let Some(state) = shell_state(tool)
    {
        suffix.push_str(&format!(" · {state}"));
    }
    if let Some(reads) = shell(tool).and_then(|shell| shell["reads"].as_u64()) {
        suffix.push_str(&format!(" · {reads} reads"));
    }
    if let Some(duration) = tool
        .duration
        .filter(|d| *d >= Duration::from_secs(1) && tool.done())
    {
        suffix.push_str(&format!(" · {}", activity::elapsed(duration)));
    }
    let mut title_style = Style::default().add_modifier(Modifier::BOLD);
    if failed && !matches!(kind, Action::Ran | Action::Called) {
        title_style = title_style.patch(theme.error);
    }
    let script = command(tool);
    // Expanded commands continue on their own rows below the title.
    let continued: Vec<&str> = if kind == Action::Ran && expanded {
        script.lines().skip(1).collect()
    } else {
        Vec::new()
    };
    let subject = match kind {
        Action::Ran => {
            let mut first = script.lines().next().unwrap_or_default().to_owned();
            if !expanded && script.contains('\n') {
                first.push_str(" …");
            }
            Span::raw(first)
        }
        Action::Called => Span::styled(subject(tool), Style::default().fg(Color::Cyan)),
        _ => Span::raw(subject(tool)),
    };
    let suffix = Span::styled(suffix, theme.dim);
    let mut title = vec![Span::styled(action, title_style), Span::raw(" "), subject];
    if continued.is_empty() {
        title.push(suffix.clone());
    }
    let prefix = match (bullet, live) {
        (false, _) => Span::raw("  "),
        (true, Some((phase, now))) => {
            let mut bullet = running_bullet(tool, theme, phase, now);
            bullet.content.to_mut().push(' ');
            bullet
        }
        (true, None) if tool.done() => Span::styled(
            "• ",
            if tool.error {
                theme.error
            } else {
                theme.success
            }
            .add_modifier(Modifier::BOLD),
        ),
        (true, None) => Span::styled("• ", theme.dim),
    };
    let mut lines = if kind == Action::Ran && !expanded {
        let mut spans = vec![prefix];
        spans.extend(title);
        vec![wrap::fit(spans, width, true)]
    } else {
        wrap(&title, width, &[prefix], &[Span::raw("  ")])
    };
    for (index, line) in continued.iter().enumerate() {
        let mut spans = vec![Span::raw((*line).to_owned())];
        if index + 1 == continued.len() {
            spans.push(suffix.clone());
        }
        lines.extend(wrap(&spans, width, &[Span::raw("  ")], &[Span::raw("  ")]));
    }
    if kind == Action::Called
        && tool
            .arguments
            .as_object()
            .is_some_and(|args| !args.is_empty())
    {
        lines.extend(wrap(
            &[Span::styled(tool.arguments.to_string(), theme.dim)],
            width,
            &[Span::raw("    ")],
            &[Span::raw("    ")],
        ));
    }
    lines.extend(details(tool, width, theme, expanded));
    lines
}

fn tree(index: usize, theme: &Theme) -> Span<'static> {
    if index == 0 {
        Span::styled("  └ ", theme.dim)
    } else {
        Span::raw("    ")
    }
}

/// The last three non-empty logical lines and a count of everything hidden; edits show the
/// start of their diff, and the transcript shows all of it.
fn details(tool: &Tool, width: usize, theme: &Theme, expanded: bool) -> Vec<Line<'static>> {
    let (mut lines, hidden) = preview(tool, width, theme, expanded);
    if hidden > 0 {
        lines.push(Line::from(vec![
            Span::raw("    "),
            Span::styled(format!("+ {}", count(hidden, "line")), theme.dim),
        ]));
    }
    lines
}

/// The rows `details` shows and how many it leaves out.
fn preview(
    tool: &Tool,
    width: usize,
    theme: &Theme,
    expanded: bool,
) -> (Vec<Line<'static>>, usize) {
    let output = tool.output.as_ref().or(tool.partial.as_ref());
    if tool.name == "tool_search"
        && !tool.error
        && !expanded
        && let Some(found) = output.and_then(|output| output["details"]["found"].as_array())
    {
        let names = found
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join(", ");
        let summary = format!(
            "Found {} tools{}",
            found.len(),
            if names.is_empty() {
                String::new()
            } else {
                format!(": {names}")
            }
        );
        return (
            wrap(
                &[Span::raw(summary)],
                width,
                &[tree(0, theme)],
                &[Span::raw("    ")],
            ),
            0,
        );
    }
    let path = argument(tool, &["path", "file_path"]);
    if let Some(text) = output.and_then(|o| o["details"]["diff"].as_str()) {
        let rows = diff::lines(&plain(text), path, width.saturating_sub(4).max(1), theme);
        let shown = if expanded {
            rows.len()
        } else {
            rows.len().min(DIFF_PREVIEW_ROWS)
        };
        let hidden = rows.len() - shown;
        let lines = rows
            .into_iter()
            .take(shown)
            .enumerate()
            .map(|(index, mut line)| {
                line.spans.insert(0, tree(index, theme));
                line
            })
            .collect();
        return (lines, hidden);
    }
    let text = match output {
        Some(output) if Action::of(tool) == Action::Shell && !tool.error => {
            match tool.name.as_str() {
                "shell_list" => output_text(output),
                // A stop or an empty read says all it needs in its title.
                "shell_stop" => return (Vec::new(), 0),
                _ => match shell_text(output) {
                    text if text.is_empty() => return (Vec::new(), 0),
                    text => text,
                },
            }
        }
        output => output.map(output_text).unwrap_or_default(),
    };
    if text.is_empty() {
        let lines = if tool.done() {
            vec![Line::from(vec![
                tree(0, theme),
                Span::styled("(no output)", theme.dim),
            ])]
        } else {
            Vec::new()
        };
        return (lines, 0);
    }
    let all: Vec<&str> = text.split('\n').collect();
    if expanded {
        let lines = all
            .iter()
            .flat_map(|line| {
                wrap(
                    &[Span::styled((*line).to_owned(), theme.dim)],
                    width.saturating_sub(4).max(1),
                    &[],
                    &[],
                )
            })
            .enumerate()
            .map(|(index, mut line)| {
                line.spans.insert(0, tree(index, theme));
                line
            })
            .collect();
        return (lines, 0);
    }
    let shown: Vec<&str> = all
        .iter()
        .copied()
        .filter(|l| !l.trim().is_empty())
        .collect();
    let shown = &shown[shown.len().saturating_sub(PREVIEW_ROWS)..];
    let room = width.saturating_sub(4).max(1);
    let cut = shown.iter().filter(|l| wrap::width(l) > room).count();
    let hidden = all.len() - shown.len() + cut;
    let lines = shown
        .iter()
        .enumerate()
        .map(|(index, line)| {
            wrap::fit(
                vec![
                    tree(index, theme),
                    Span::styled((*line).to_owned(), theme.dim),
                ],
                width,
                false,
            )
        })
        .collect();
    (lines, hidden)
}

/// What expanding a finished group reveals: the output rows its preview leaves out, or other
/// details the transcript view adds.
pub(crate) fn fold(tools: &[Tool], width: usize, theme: &Theme) -> Option<Fold> {
    if let [tool] = tools
        && let Some(inner) = delegated(tool)
    {
        return fold(&[inner], width, theme);
    }
    if tools.iter().any(|tool| !tool.done()) {
        return None;
    }
    match tools {
        [tool] if maintenance(tool).is_none() && !exploration(tool) => {
            let custom = custom(tool, width, theme, View::History);
            // Plain tools end in the preview's `+ N lines` row, which becomes the control.
            if custom.is_none() && preview(tool, width, theme, false).1 > 0 {
                return Some(Fold::Lines);
            }
            let history = custom.unwrap_or_else(|| single(tool, width, theme, View::History, true));
            let text = |lines: Vec<Line<'static>>| -> Vec<String> {
                lines
                    .iter()
                    .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                    .collect()
            };
            (text(history) != text(render(tools, width, theme, View::Transcript)))
                .then_some(Fold::Details)
        }
        _ => Some(Fold::Details),
    }
}

fn delegated(tool: &Tool) -> Option<Tool> {
    if tool.name != "call_tool" {
        return None;
    }
    let name = tool.arguments["name"].as_str()?;
    // Malformed historical wrappers must not recurse into themselves.
    if name == "call_tool" {
        return None;
    }
    Some(Tool {
        name: name.into(),
        arguments: tool
            .arguments
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({})),
        ..tool.clone()
    })
}

/// Tools with a presentation of their own.
fn custom(tool: &Tool, width: usize, theme: &Theme, view: View) -> Option<Vec<Line<'static>>> {
    match tool.name.as_str() {
        "call_tool" => {
            let inner = delegated(tool)?;
            Some(render(&[inner], width, theme, view))
        }
        "subagent" => Some(crate::subagents::render(tool, width, theme, view)),
        "run_code" => Some(crate::program::render(tool, width, theme, view)),
        "create_goal" | "get_goal" | "update_goal" => {
            let text = tool
                .output
                .as_ref()
                .and_then(|o| o["content"].as_array())
                .into_iter()
                .flatten()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("\n");
            Some(crate::goal::report(&text, width, theme))
        }
        "request_user_input" => Some(questions(tool, width, theme)),
        _ => None,
    }
}

/// One group from `groups`: an Explored group or a single tool.
pub fn render(tools: &[Tool], width: usize, theme: &Theme, view: View) -> Vec<Line<'static>> {
    if let [tool] = tools
        && let Some(lines) = custom(tool, width, theme, view)
    {
        return lines;
    }
    match tools {
        [first, ..] if maintenance(first).is_some() => maintained(tools, width, theme, view),
        [first, _, ..] if poll(first).is_some() => polled(tools, width, theme, view),
        [tool] if !exploration(tool) => single(tool, width, theme, view, true),
        _ => explored(tools, width, theme, view),
    }
}

/// Consecutive reads of one shell task as one read: the latest state, the output they
/// returned between them, and the time spent; the transcript shows each read.
fn polled(tools: &[Tool], width: usize, theme: &Theme, view: View) -> Vec<Line<'static>> {
    if matches!(view, View::Transcript) {
        return tools
            .iter()
            .flat_map(|tool| single(tool, width, theme, view, true))
            .collect();
    }
    let last = &tools[tools.len() - 1];
    let latest = tools.iter().rev().find_map(shell);
    let text: Vec<String> = tools
        .iter()
        .filter_map(|tool| tool.output.as_ref())
        .map(shell_text)
        .filter(|text| !text.is_empty())
        .collect();
    let mut shell = latest.cloned().unwrap_or_else(|| serde_json::json!({}));
    // Only merged reads carry a count, shown after the state.
    shell["reads"] = tools.len().into();
    let output = serde_json::json!({
        "content": [],
        "is_error": false,
        "details": {"shell": shell},
        "structured_content": {"output": text.join("\n")},
    });
    let mut arguments = last.arguments.clone();
    if let Some(command) = latest.and_then(|shell| shell["command"].as_str()) {
        // The running read has no result to name the command yet.
        arguments["command"] = command.into();
    }
    let merged = Tool {
        arguments,
        output: last.done().then(|| output.clone()),
        partial: (!last.done()).then_some(output),
        error: false,
        duration: last
            .done()
            .then(|| tools.iter().filter_map(|tool| tool.duration).sum()),
        ..last.clone()
    };
    single(&merged, width, theme, view, true)
}

fn questions(tool: &Tool, width: usize, theme: &Theme) -> Vec<Line<'static>> {
    let Some(output) = &tool.output else {
        return vec![];
    };
    let details = &output["details"];
    let questions = details["questions"]
        .as_array()
        .or_else(|| tool.arguments["questions"].as_array())
        .into_iter()
        .flatten()
        .filter(|question| question["id"].is_string() && question["question"].is_string())
        .collect::<Vec<_>>();
    let answers = |question: &Value| {
        details["response"]["answers"][question["id"].as_str().unwrap_or_default()]["answers"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
    };
    let count = questions
        .iter()
        .filter(|question| !answers(question).is_empty())
        .count();
    let message = details["message"]
        .as_str()
        .map(str::to_owned)
        .or_else(|| tool.error.then(|| output_text(output)));
    let cancelled = details["status"] == "cancelled"
        || (details["status"].is_null()
            && tool.error
            && message.as_ref().is_some_and(|message| {
                let message = message.to_ascii_lowercase();
                ["abort", "cancel", "interrupt"]
                    .iter()
                    .any(|word| message.contains(word))
            }));
    let cyan = Style::default().fg(Color::Cyan);
    let mut header = vec![
        Span::styled("• ", theme.dim),
        Span::styled("Questions", Style::default().add_modifier(Modifier::BOLD)),
        Span::styled(format!(" {count}/{} answered", questions.len()), theme.dim),
    ];
    if cancelled {
        header.push(Span::styled(" (interrupted)", cyan));
    } else if details["status"] == "timed_out" {
        header.push(Span::styled(" (timed out)", cyan));
    }
    let mut rows = wrap(&header, width, &[], &[]);
    for question in questions {
        let values = answers(question);
        let mut prompt = vec![Span::raw(plain(
            question["question"].as_str().unwrap_or_default(),
        ))];
        if values.is_empty() {
            prompt.push(Span::styled(" (unanswered)", theme.dim));
        }
        rows.extend(wrap(
            &prompt,
            width,
            &[Span::raw("  • ")],
            &[Span::raw("    ")],
        ));
        for answer in values {
            let note = answer.strip_prefix("user_note: ");
            let label = if note.is_some() && question["options"].is_array() {
                "note: "
            } else {
                "answer: "
            };
            rows.extend(wrap(
                &[Span::styled(plain(note.unwrap_or(answer)), cyan)],
                width,
                &[Span::raw("    "), Span::styled(label, theme.dim)],
                &[Span::raw(format!("    {}", " ".repeat(label.len())))],
            ));
        }
    }
    if let Some(message) = message {
        let style = if cancelled {
            cyan.add_modifier(Modifier::DIM)
        } else {
            theme.error
        };
        rows.extend(wrap(
            &[Span::styled(plain(&message), style)],
            width,
            &[Span::styled("  ↳ ", style)],
            &[Span::styled("    ", style)],
        ));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn delegated_output_uses_the_inner_fold_control() {
        let theme = Theme::fixture(crate::palette::ColorLevel::TrueColor, false);
        let direct = tool(
            "mcp__fixture__echo",
            json!({}),
            Some(("one\ntwo\nthree\nfour\nfive", false)),
        );
        let wrapped = Tool {
            name: "call_tool".into(),
            arguments: json!({"name":direct.name,"arguments":direct.arguments}),
            ..direct.clone()
        };
        assert_eq!(fold(&[direct], 80, &theme), Some(Fold::Lines));
        assert_eq!(fold(&[wrapped], 80, &theme), Some(Fold::Lines));
    }

    #[test]
    fn explored_names_shorten_until_they_differ() {
        let paths = [
            "docs/README.md",
            "README.md",
            "src/app/mod.rs",
            "src/ui/mod.rs",
            ".",
        ];
        assert_eq!(
            short_names(&paths.map(String::from)),
            [
                "docs/README.md",
                "README.md",
                "app/mod.rs",
                "ui/mod.rs",
                "."
            ]
        );
    }

    #[test]
    fn explored_steps_hang_under_their_subjects_up_to_three_rows() {
        let theme = Theme::fixture(crate::palette::ColorLevel::TrueColor, false);
        let tools: Vec<Tool> = (0..12)
            .map(|n| {
                tool(
                    "read",
                    json!({"path": format!("deep/dir/file{n:02}.rs")}),
                    Some(("", false)),
                )
            })
            .chain([tool(
                "grep",
                json!({"pattern": "todo", "path": "src"}),
                Some(("", false)),
            )])
            .collect();
        let text = |width| -> Vec<String> {
            explored(&tools, width, &theme, View::History)
                .iter()
                .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
                .collect()
        };
        assert_eq!(
            text(40),
            [
                "• Explored",
                "  └ Read file00.rs, file01.rs,",
                "         file02.rs, file03.rs,",
                "         file04.rs, file05.rs, …",
            ]
        );
        assert_eq!(
            text(200)[1..],
            [
                format!(
                    "  └ Read {}",
                    (0..12)
                        .map(|n| format!("file{n:02}.rs"))
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
                "    Search todo in src".to_owned(),
            ]
        );
    }

    fn tool(name: &str, arguments: Value, output: Option<(&str, bool)>) -> Tool {
        let mut tool = Tool::new(name.into(), name.into(), arguments);
        if let Some((text, error)) = output {
            tool.output =
                Some(json!({"content": [{"kind": "text", "text": text}], "is_error": error}));
            tool.error = error;
        }
        tool
    }

    #[test]
    fn explorations_merge_until_an_error_or_another_tool() {
        let tools = [
            tool("read", json!({"path": "a"}), Some(("x", false))),
            tool("grep", json!({"pattern": "p"}), Some(("x", false))),
            tool("read", json!({"path": "b"}), Some(("missing", true))),
            tool("read", json!({"path": "c"}), None),
            tool("bash", json!({"command": "ls"}), None),
            tool("ls", json!({}), None),
        ];
        assert_eq!(groups(&tools), [0..2, 2..3, 3..4, 4..5, 5..6]);
    }

    #[test]
    fn history_tools_merge_apart_from_explorations_and_keep_failures() {
        let tools = [
            tool("read", json!({"path": "a"}), Some(("x", false))),
            tool("context_status", json!({}), Some(("{}", false))),
            tool("compact_history", json!({}), Some(("{}", true))),
            tool("search_history", json!({}), None),
            tool("grep", json!({"pattern": "p"}), None),
            tool("read_history", json!({}), None),
        ];
        assert_eq!(groups(&tools), [0..1, 1..4, 4..5, 5..6]);
    }

    fn shell_tool(name: &str, arguments: Value, text: &str, shell: Option<Value>) -> Tool {
        let mut tool = tool(name, arguments, Some((text, false)));
        if let Some(shell) = shell {
            tool.output.as_mut().unwrap()["details"] = json!({ "shell": shell });
        }
        tool
    }

    fn task(state: &str, new_output: Option<bool>) -> Value {
        let mut shell = json!({
            "task_id": "sh1", "command": "npm run dev\n  --port 3000", "state": state, "exit_code": null,
        });
        if let Some(new_output) = new_output {
            shell["new_output"] = json!(new_output);
        }
        shell
    }

    fn rows(tools: &[Tool], view: View) -> Vec<String> {
        let theme = Theme::fixture(crate::palette::ColorLevel::TrueColor, false);
        render(tools, 80, &theme, view)
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    #[test]
    fn shell_tools_name_their_task_without_arguments_or_status_lines() {
        let read = shell_tool(
            "shell_output",
            json!({"task_id":"sh1","wait_ms":5000}),
            "one\ntwo\n\n[sh1 · running · cursor 8]",
            Some(task("running", Some(true))),
        );
        assert_eq!(
            rows(&[read], View::History),
            [
                "• Output sh1 · npm run dev … · running",
                "  └ one",
                "    two"
            ]
        );
        let empty = shell_tool(
            "shell_output",
            json!({"task_id":"sh1"}),
            "(no new output)\n\n[sh1 · running · cursor 8]",
            Some(task("running", Some(false))),
        );
        assert_eq!(
            rows(&[empty], View::History),
            ["• Waited sh1 · npm run dev … · running"]
        );
        // Sessions from before the details only have the arguments and the text.
        let legacy = shell_tool(
            "shell_output",
            json!({"task_id":"sh1","wait_ms":5000}),
            "(no new output)\n\n[sh1 · running · cursor 8]",
            None,
        );
        assert_eq!(rows(&[legacy], View::History), ["• Waited sh1"]);
        let mut stop = task("cancelled", None);
        stop["command"] = json!("sleep 30");
        let stop = shell_tool(
            "shell_stop",
            json!({"task_id":"sh1"}),
            "sh1 cancelled · 0s.",
            Some(stop),
        );
        assert_eq!(
            rows(&[stop], View::History),
            ["• Stopped sh1 · sleep 30 · cancelled"]
        );
        let list = shell_tool(
            "shell_list",
            json!({}),
            "sh1  background  running  00:02  npm run dev",
            None,
        );
        assert_eq!(
            rows(&[list], View::History),
            [
                "• Listed shells",
                "  └ sh1  background  running  00:02  npm run dev"
            ]
        );
    }

    #[test]
    fn background_starts_show_early_output_unless_the_command_already_ended() {
        let arguments = json!({"command":"npm run dev","background":true});
        let mut started = task("running", Some(true));
        started["command"] = json!("npm run dev");
        let start = shell_tool(
            "bash",
            arguments.clone(),
            "ready\n\n[sh1 · started in the background · cursor 5 · you will be told when it finishes]",
            Some(started),
        );
        assert_eq!(
            rows(&[start], View::History),
            ["• Started sh1 · npm run dev", "  └ ready"]
        );
        let ended = shell_tool("bash", arguments.clone(), "done", None);
        assert_eq!(
            rows(&[ended], View::History),
            ["• Ran npm run dev", "  └ done"]
        );
        let mut live = tool("bash", arguments, None);
        live.started = Some(Instant::now());
        let view = View::Live {
            phase: Duration::ZERO,
            now: Instant::now(),
        };
        assert!(rows(&[live], view)[0].ends_with(" Starting npm run dev"));
    }

    #[test]
    fn consecutive_reads_of_one_task_merge_into_one_wait() {
        let read = |text: &str, new_output: bool, seconds: u64| {
            let mut tool = shell_tool(
                "shell_output",
                json!({"task_id":"sh1","wait_ms":5000}),
                &format!("{text}\n\n[sh1 · running · cursor 1]"),
                Some(task("running", Some(new_output))),
            );
            tool.duration = Some(Duration::from_secs(seconds));
            tool
        };
        let mut other = read("x", true, 0);
        other.arguments = json!({"task_id":"sh2"});
        other.output.as_mut().unwrap()["details"]["shell"]["task_id"] = json!("sh2");
        let mut failed = tool(
            "shell_output",
            json!({"task_id":"sh1"}),
            Some(("No shell task sh1", true)),
        );
        failed.error = true;
        let tools = [
            read("(no new output)", false, 2),
            read("compiled", true, 3),
            read("(no new output)", false, 4),
            other,
            failed,
            read("(no new output)", false, 0),
        ];
        assert_eq!(groups(&tools), [0..3, 3..4, 4..5, 5..6]);
        assert_eq!(
            rows(&tools[..3], View::History),
            [
                "• Output sh1 · npm run dev … · running · 3 reads · 9s",
                "  └ compiled"
            ]
        );
        assert_eq!(
            rows(&[tools[0].clone(), tools[2].clone()], View::History),
            ["• Waited sh1 · npm run dev … · running · 2 reads · 6s"]
        );
        let transcript = rows(&tools[..3], View::Transcript);
        assert_eq!(
            transcript
                .iter()
                .filter(|row| row.starts_with("• "))
                .count(),
            3
        );
        let theme = Theme::fixture(crate::palette::ColorLevel::TrueColor, false);
        assert_eq!(fold(&tools[..3], 80, &theme), Some(Fold::Details));
        let mut waiting = tool(
            "shell_output",
            json!({"task_id":"sh1","wait_ms":5000}),
            None,
        );
        waiting.started = Some(Instant::now());
        let view = View::Live {
            phase: Duration::ZERO,
            now: Instant::now(),
        };
        let live = rows(&[tools[1].clone(), waiting], view);
        assert!(
            live[0].ends_with(" Waiting sh1 · npm run dev … · running"),
            "{live:?}"
        );
        assert_eq!(live[1..], ["  └ compiled"]);
    }

    #[test]
    fn control_sequences_are_stripped() {
        assert_eq!(
            plain("\x1b[31mred\x1b[0m\r\n\x1b]0;title\x07ok\tz"),
            "red\nok    z"
        );
    }
}
