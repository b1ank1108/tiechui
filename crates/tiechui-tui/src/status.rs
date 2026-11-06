//! The status line under the composer: items with real values only, in text or Nerd
//! Font form, the context battery, and softened syntax-theme colors.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::{Line, Span};
use tiechui_protocol::{ModelInfo, ThinkingLevel, TokenUsage};

pub use tiechui_config::{PermissionMode, StatusLineConfig, StatusLineItem as Item};

use crate::palette::{ColorLevel, Rgb, best_color};
use crate::syntax;
use crate::theme::Theme;
use crate::wrap;

const SEPARATOR: &str = " · ";
const BATTERIES: [&str; 5] = ["\u{f244}", "\u{f243}", "\u{f242}", "\u{f241}", "\u{f240}"];
const REASONING_GLYPH: &str = "\u{ea61}";
const COMPACTED_GLYPH: &str = "\u{eac5}";
const GIT_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Ready,
    Thinking,
    Working,
    Waiting,
    Compacting,
    Error,
    Cancelled,
}

impl Phase {
    fn label(self) -> &'static str {
        match self {
            Self::Ready => "Ready",
            Self::Thinking => "Thinking",
            Self::Working => "Working",
            Self::Waiting => "Waiting",
            Self::Compacting => "Compacting",
            Self::Error => "Error",
            Self::Cancelled => "Cancelled",
        }
    }

    fn glyph(self) -> &'static str {
        match self {
            Self::Ready => "\u{eabc}",
            Self::Thinking => "\u{eb19}",
            Self::Working => "\u{eb2c}",
            Self::Waiting => "\u{ead1}",
            Self::Compacting => COMPACTED_GLYPH,
            Self::Error => "\u{ea87}",
            Self::Cancelled => "\u{eba5}",
        }
    }
}

/// Git facts; each is fetched only while an item needs it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Git {
    pub project: Option<String>,
    pub branch: Option<String>,
    pub changes: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GitFields {
    pub project: bool,
    pub branch: bool,
    pub changes: bool,
}

impl GitFields {
    pub fn of(items: &[Item]) -> Self {
        Self {
            project: items.contains(&Item::ProjectName),
            branch: items.contains(&Item::GitBranch),
            changes: items.contains(&Item::BranchChanges),
        }
    }

    pub fn any(self) -> bool {
        self.project || self.branch || self.changes
    }
}

/// Everything the items read, all from the session's own events.
#[derive(Debug, Clone, Copy)]
pub struct Snapshot<'a> {
    pub cwd: &'a Path,
    pub home: Option<&'a Path>,
    pub hostname: Option<&'a str>,
    pub model: Option<&'a ModelInfo>,
    pub thinking: ThinkingLevel,
    /// The latest request's usage on the current branch.
    pub context: Option<&'a TokenUsage>,
    /// Cumulative usage for the session.
    pub total: &'a TokenUsage,
    /// A checkpoint replaced the context and no request has measured it since.
    pub compacted: bool,
    pub phase: Phase,
    pub permission: PermissionMode,
    pub thread: Option<&'a str>,
    pub git: &'a Git,
}

impl Snapshot<'_> {
    /// Rounded percent of the context window in use; zero before the
    /// session's first request.
    fn used(&self) -> Option<u64> {
        let window = u128::from(self.model?.context_window);
        if window == 0 {
            return None;
        }
        let Some(usage) = self.context else {
            return (!self.compacted && *self.total == TokenUsage::default()).then_some(0);
        };
        let tokens = u128::from(usage.input + usage.output + usage.cache_read + usage.cache_write);
        let percent = (tokens * 200 + window) / (window * 2);
        Some(u64::try_from(percent.min(100)).unwrap_or(100))
    }

    fn window(&self) -> Option<u64> {
        self.model
            .map(|model| model.context_window)
            .filter(|window| *window > 0)
    }

    fn input(&self) -> u64 {
        self.total.input + self.total.cache_read + self.total.cache_write
    }

    /// Cache reads over all input, cache writes included.
    fn cache_rate(&self) -> Option<String> {
        let input = u128::from(self.input());
        if input == 0 {
            return None;
        }
        let tenths = (u128::from(self.total.cache_read) * 2000 + input) / (input * 2);
        Some(format!("{}%", tenths_text(tenths)))
    }

    fn thinking(&self) -> &'static str {
        thinking_name(self.thinking)
    }
}

pub fn thinking_name(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off => "off",
        ThinkingLevel::Minimal => "minimal",
        ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh => "xhigh",
        ThinkingLevel::Max => "max",
    }
}

fn tenths_text(tenths: u128) -> String {
    match tenths % 10 {
        0 => format!("{}", tenths / 10),
        digit => format!("{}.{digit}", tenths / 10),
    }
}

/// `1.2k`, `3M`: one decimal, a trailing `.0` dropped.
pub fn compact_tokens(value: u64) -> String {
    let value = u128::from(value);
    if value >= 1_000_000 {
        format!("{}M", tenths_text((value + 50_000) / 100_000))
    } else if value >= 1_000 {
        format!("{}k", tenths_text((value + 50) / 100))
    } else {
        value.to_string()
    }
}

/// Empty is reserved for zero and full for 100%, never for 76%.
pub fn battery(remaining: Option<u64>) -> &'static str {
    let level = match remaining {
        None | Some(0) => 0,
        Some(100..) => 4,
        Some(remaining) => remaining.div_ceil(25).min(3),
    };
    BATTERIES[usize::try_from(level).unwrap_or(0)]
}

fn single_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `~` for the home directory and paths below it.
pub(crate) fn compact_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(child) if child.as_os_str().is_empty() => "~".into(),
        Some(child) => format!("~/{}", child.display()),
        None => path.display().to_string(),
    }
}

pub fn description(item: Item) -> &'static str {
    match item {
        Item::Model => "Current model name",
        Item::ModelWithReasoning => "Current model name with reasoning level",
        Item::Reasoning => "Current reasoning level",
        Item::CurrentDir => "Current working directory",
        Item::ProjectName => "Project name (when available)",
        Item::Hostname => "Current machine hostname",
        Item::GitBranch => "Current Git branch (when available)",
        Item::BranchChanges => "Committed changes against the default branch",
        Item::RunState => "Current session state",
        Item::PermissionMode => "Permission mode, shown while not rules",
        Item::ContextRemaining => "Percentage of context window remaining",
        Item::ContextUsed => "Percentage of context window used",
        Item::Version => "Tiechui version",
        Item::ContextWindowSize => "Context window size in tokens",
        Item::UsedTokens => "Total session tokens (omitted when zero)",
        Item::TotalInputTokens => "Total session input tokens",
        Item::TotalOutputTokens => "Total session output tokens",
        Item::CacheHitRate => "Session cache reads / total input",
        Item::EstimatedThreadCost => "Estimated session cost (when prices are configured)",
        Item::ThreadId => "Current session identifier",
        Item::ThreadTitle => "Session title, or its identifier when unnamed",
    }
}

/// Nerd Fonts v3 Codicons, with Font Awesome's five-stage battery.
pub fn glyph(item: Item) -> &'static str {
    match item {
        Item::Model | Item::ModelWithReasoning => "\u{ec19}",
        Item::Reasoning => REASONING_GLYPH,
        Item::CurrentDir => "\u{eaf7}",
        Item::ProjectName => "\u{ea62}",
        Item::Hostname => "\u{eb50}",
        Item::GitBranch => "\u{ea68}",
        Item::BranchChanges => "\u{eae1}",
        Item::RunState => "\u{eabc}",
        Item::PermissionMode => "⏵⏵",
        Item::ContextRemaining => BATTERIES[0],
        Item::ContextUsed => "\u{ebe4}",
        Item::ContextWindowSize => "\u{eaae}",
        Item::UsedTokens => "\u{ebd2}",
        Item::TotalInputTokens => "\u{ea9a}",
        Item::TotalOutputTokens => "\u{eaa1}",
        Item::CacheHitRate => "\u{eace}",
        Item::EstimatedThreadCost => "\u{eac9}",
        Item::Version => "\u{ea66}",
        Item::ThreadId => "#",
        Item::ThreadTitle => "\u{ea6b}",
    }
}

/// The text form, or `None` when there is no real value.
pub fn text(item: Item, s: &Snapshot<'_>) -> Option<String> {
    let model = || s.model.map(|model| single_line(&model.id));
    let reasoning = s.model.is_some_and(|model| model.reasoning);
    let value = match item {
        Item::Model => model()?,
        Item::ModelWithReasoning if reasoning => format!("{} ({})", model()?, s.thinking()),
        Item::ModelWithReasoning => model()?,
        Item::Reasoning => {
            s.model?;
            if reasoning { s.thinking() } else { "none" }.into()
        }
        Item::CurrentDir => single_line(&compact_path(s.cwd, s.home)),
        Item::ProjectName => s.git.project.clone()?,
        Item::Hostname => single_line(s.hostname?),
        Item::GitBranch => s.git.branch.clone()?,
        Item::BranchChanges => s.git.changes.clone()?,
        Item::RunState => s.phase.label().into(),
        Item::PermissionMode => match s.permission {
            PermissionMode::Rules => return None,
            PermissionMode::Auto => "⏵⏵ auto".into(),
            PermissionMode::Yolo => "⏵⏵ yolo".into(),
        },
        Item::ContextRemaining | Item::ContextUsed if s.compacted => "◇ compacted".into(),
        Item::ContextRemaining => format!("{}% left", 100 - s.used()?),
        Item::ContextUsed => format!("{}% used", s.used()?),
        Item::Version => env!("CARGO_PKG_VERSION").into(),
        Item::ContextWindowSize => compact_tokens(s.window()?),
        Item::UsedTokens => {
            let total = s.input() + s.total.output;
            (total > 0).then(|| format!("{} used", compact_tokens(total)))?
        }
        Item::TotalInputTokens => format!("{} in", compact_tokens(s.input())),
        Item::TotalOutputTokens => format!("{} out", compact_tokens(s.total.output)),
        Item::CacheHitRate => format!("{} cache", s.cache_rate()?),
        Item::EstimatedThreadCost => {
            format!("~${:.2}", s.total.cost_nano_usd? as f64 / 1_000_000_000.0)
        }
        Item::ThreadId | Item::ThreadTitle => s.thread?.to_owned(),
    };
    (!value.is_empty()).then_some(value)
}

/// The glyph form; the battery shows `--%` while the context is unknown.
fn icon(item: Item, s: &Snapshot<'_>) -> Option<String> {
    let compact = match item {
        Item::ContextRemaining | Item::ContextUsed if s.compacted => {
            return Some(format!("{COMPACTED_GLYPH} compacted"));
        }
        Item::ContextRemaining => {
            let remaining = s.used().map(|used| 100 - used);
            let value = remaining.map_or_else(|| "--%".into(), |r| format!("{r}%"));
            return Some(format!("{} {value}", battery(remaining)));
        }
        Item::RunState => return Some(format!("{} {}", s.phase.glyph(), s.phase.label())),
        Item::PermissionMode => return text(item, s),
        Item::ContextUsed => format!("{}%", s.used()?),
        Item::UsedTokens => {
            text(item, s)?;
            compact_tokens(s.input() + s.total.output)
        }
        Item::TotalInputTokens => compact_tokens(s.input()),
        Item::TotalOutputTokens => compact_tokens(s.total.output),
        Item::CacheHitRate => s.cache_rate()?,
        _ => text(item, s)?,
    };
    Some(format!("{} {compact}", glyph(item)))
}

/// The Mist role of an item's value; low context and failures take the alert tones.
fn style(item: Item, s: &Snapshot<'_>, theme: &Theme) -> Style {
    match item {
        Item::ContextRemaining | Item::ContextUsed if s.compacted => theme.accent2,
        Item::ContextRemaining => match s.used().map(|used| 100 - used) {
            Some(0..=10) => theme.error,
            Some(11..=20) => theme.warning,
            _ => theme.secondary,
        },
        Item::ContextUsed => match s.used() {
            Some(90..) => theme.error,
            Some(80..=89) => theme.warning,
            _ => theme.secondary,
        },
        Item::RunState => match s.phase {
            Phase::Error => theme.error,
            Phase::Waiting => theme.warning,
            Phase::Thinking | Phase::Working | Phase::Compacting => theme.accent2,
            Phase::Ready | Phase::Cancelled => theme.dim,
        },
        Item::PermissionMode => match s.permission {
            PermissionMode::Yolo => theme.error.add_modifier(Modifier::BOLD),
            _ => theme.warning,
        },
        Item::Model | Item::ModelWithReasoning | Item::CurrentDir | Item::ProjectName => {
            theme.secondary
        }
        Item::GitBranch | Item::ThreadTitle => theme.accent,
        _ => theme.dim,
    }
}

/// Identity and environment sit on the left; state, context and usage on the right.
fn is_left(item: Item) -> bool {
    matches!(
        item,
        Item::Model
            | Item::ModelWithReasoning
            | Item::Reasoning
            | Item::CurrentDir
            | Item::ProjectName
            | Item::GitBranch
            | Item::BranchChanges
            | Item::Hostname
            | Item::Version
            | Item::ThreadId
            | Item::ThreadTitle
    )
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Accent {
    Model,
    Path,
    Branch,
    State,
    Usage,
    Metadata,
    Thread,
}

impl Accent {
    const ALL: [Self; 7] = [
        Self::Model,
        Self::Path,
        Self::Branch,
        Self::State,
        Self::Usage,
        Self::Metadata,
        Self::Thread,
    ];

    /// Syntax scopes to try; the first with a foreground wins.
    fn scopes(self) -> &'static [&'static str] {
        match self {
            Self::Model => &["entity.name.type", "support.type", "variable"],
            Self::Path => &["string", "markup.underline.link"],
            Self::Branch => &["entity.name.function", "entity.name.tag"],
            Self::State => &["keyword.control", "keyword"],
            Self::Usage => &["constant.numeric", "constant"],
            Self::Metadata => &["comment", "constant.other"],
            Self::Thread => &["markup.heading", "entity.name.section"],
        }
    }

    fn fallback(self) -> Color {
        match self {
            Self::Model | Self::State | Self::Metadata => Color::Cyan,
            Self::Path | Self::Usage => Color::Green,
            Self::Branch | Self::Thread => Color::Magenta,
        }
    }
}

/// Integer weighted luma and 85% saturation, without dimming.
fn soften(Rgb(r, g, b): Rgb) -> Rgb {
    let luma = (77 * u16::from(r) + 150 * u16::from(g) + 29 * u16::from(b)) / 256;
    let channel = |c: u8| u8::try_from((u16::from(c) * 85 + luma * 15 + 50) / 100).unwrap_or(255);
    Rgb(channel(r), channel(g), channel(b))
}

fn accent_colors(theme: &Theme) -> [Option<Rgb>; 7] {
    static DARK: OnceLock<[Option<Rgb>; 7]> = OnceLock::new();
    static LIGHT: OnceLock<[Option<Rgb>; 7]> = OnceLock::new();
    let resolve = || Accent::ALL.map(|accent| syntax::scope_foreground(accent.scopes(), theme));
    if theme.syntax.is_some() {
        resolve()
    } else if theme.palette.light() {
        *LIGHT.get_or_init(resolve)
    } else {
        *DARK.get_or_init(resolve)
    }
}

fn accent_style(accent: Accent, theme: &Theme) -> Style {
    let index = Accent::ALL.iter().position(|a| *a == accent).unwrap_or(0);
    let color = accent_colors(theme)[index]
        .map(soften)
        .and_then(|rgb| best_color(rgb, theme.palette.level))
        .unwrap_or_else(|| accent.fallback());
    Style::default().fg(color)
}

pub(crate) fn category_style(category: &str, theme: &Theme) -> Style {
    let accent = match category {
        "System prompt" => Accent::Model,
        "Instructions" => Accent::Path,
        "Skills" => Accent::State,
        "Tool definitions" => Accent::Branch,
        "Assistant" => Accent::Usage,
        "Tool results" => Accent::Metadata,
        _ => Accent::Thread,
    };
    accent_style(accent, theme)
}

/// Whether theme colors apply at all: the setting, and a terminal with colors.
pub fn colored(config: &StatusLineConfig, theme: &Theme) -> bool {
    config.use_theme_colors && theme.palette.level != ColorLevel::Unknown
}

/// One item in its configured form, or nothing without a real value.
pub fn segment(
    item: Item,
    s: &Snapshot<'_>,
    theme: &Theme,
    use_colors: bool,
    use_icon: bool,
) -> Option<Vec<Span<'static>>> {
    let value = if use_icon {
        icon(item, s)?
    } else {
        text(item, s)?
    };
    let style = if use_colors {
        self::style(item, s, theme)
    } else {
        theme.dim
    };
    // The reasoning level is a dim suffix of the model name.
    let suffix = format!(" ({})", s.thinking());
    if item == Item::ModelWithReasoning
        && let Some(name) = value.strip_suffix(&suffix)
    {
        return Some(vec![
            Span::styled(name.to_owned(), style),
            Span::styled(suffix, theme.dim),
        ]);
    }
    Some(vec![Span::styled(value, style)])
}

/// The configured items with values, split into columns; a context percentage absorbs an
/// adjacent window size as `75% left / 200k`.
type Segments = Vec<Vec<Span<'static>>>;

fn columns(config: &StatusLineConfig, s: &Snapshot<'_>, theme: &Theme) -> (Segments, Segments) {
    let use_colors = colored(config, theme);
    let (mut left, mut right) = (Vec::new(), Vec::new());
    let mut items = config.items.iter().copied().peekable();
    while let Some(item) = items.next() {
        let Some(mut segment) = segment(item, s, theme, use_colors, config.icons.contains(&item))
        else {
            continue;
        };
        if matches!(item, Item::ContextRemaining | Item::ContextUsed)
            && !s.compacted
            && items.peek() == Some(&Item::ContextWindowSize)
            && let Some(window) = s.window()
        {
            items.next();
            segment.push(Span::styled(
                format!(" / {}", compact_tokens(window)),
                theme.dim,
            ));
        }
        if is_left(item) {
            left.push(segment);
        } else {
            right.push(segment);
        }
    }
    (left, right)
}

fn joined(segments: &[Vec<Span<'static>>], theme: &Theme) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    for segment in segments {
        if !spans.is_empty() {
            spans.push(Span::styled(SEPARATOR, theme.dim));
        }
        spans.extend(segment.iter().cloned());
    }
    spans
}

fn spans_width(spans: &[Span<'_>]) -> usize {
    spans.iter().map(|span| wrap::width(&span.content)).sum()
}

/// The configured items with values as one flow, left column first; empty when none has one.
pub fn spans(config: &StatusLineConfig, s: &Snapshot<'_>, theme: &Theme) -> Vec<Span<'static>> {
    let (mut left, right) = columns(config, s, theme);
    left.extend(right);
    joined(&left, theme)
}

/// The row under the composer, indented by two: identity on the left, state on the right.
/// Narrow screens drop trailing left fields, then shorten the left column. Without values,
/// the shortcut hint and the context remaining.
pub fn line(
    config: &StatusLineConfig,
    s: &Snapshot<'_>,
    width: usize,
    theme: &Theme,
) -> Line<'static> {
    let usable = width.saturating_sub(4).max(1);
    let (mut left, right) = columns(config, s, theme);
    let (left, right) = if left.is_empty() && right.is_empty() {
        let hint = "? for shortcuts";
        let context = s
            .used()
            .map(|used| format!("{}% context left", 100 - used))
            .filter(|text| wrap::width(hint) + 2 + wrap::width(text) <= usable)
            .into_iter()
            .map(|text| Span::styled(text, theme.dim))
            .collect();
        (vec![Span::styled(hint, theme.dim)], context)
    } else {
        let right = joined(&right, theme);
        let room = usable.saturating_sub(spans_width(&right) + 2);
        while left.len() > 1 && spans_width(&joined(&left, theme)) > room {
            left.pop();
        }
        (joined(&left, theme), right)
    };
    let (left_width, right_width) = (spans_width(&left), spans_width(&right));
    let content = if right.is_empty() {
        left
    } else if left_width + 2 + right_width <= usable {
        let mut spans = left;
        spans.push(Span::raw(" ".repeat(usable - left_width - right_width)));
        spans.extend(right);
        spans
    } else if !left.is_empty() && usable >= right_width + 2 + 8 {
        let mut spans = wrap::fit(left, usable - right_width - 2, true).spans;
        spans.push(Span::raw("  "));
        spans.extend(right);
        spans
    } else {
        right
    };
    let mut spans = vec![Span::raw("  ")];
    spans.extend(wrap::fit(content, usable, true).spans);
    wrap::fit(spans, width, true)
}

/// Runs only the git commands `fields` needs, through `run` (the arguments after `git -C`).
pub async fn collect<F, R>(fields: GitFields, run: F) -> Git
where
    F: Fn(Vec<String>) -> R,
    R: Future<Output = Option<String>>,
{
    let run = |args: &[&str]| run(args.iter().map(|arg| (*arg).to_owned()).collect());
    let (root, head, base) = tokio::join!(
        async {
            if fields.project {
                run(&["rev-parse", "--show-toplevel"]).await
            } else {
                None
            }
        },
        async {
            if fields.branch {
                run(&["rev-parse", "--abbrev-ref", "HEAD"]).await
            } else {
                None
            }
        },
        async {
            if fields.changes {
                run(&[
                    "symbolic-ref",
                    "--quiet",
                    "--short",
                    "refs/remotes/origin/HEAD",
                ])
                .await
            } else {
                None
            }
        },
    );
    let mut changes = None;
    if fields.changes {
        let references = base.map_or_else(
            || {
                ["origin/main", "origin/master", "main", "master"]
                    .map(String::from)
                    .to_vec()
            },
            |base| vec![base],
        );
        for reference in references {
            let range = format!("{reference}...HEAD");
            if let Some(numstat) = run(&["diff", "--numstat", &range, "--"]).await {
                let (mut added, mut removed) = (0u64, 0u64);
                for row in numstat.lines() {
                    let mut columns = row.split('\t');
                    let mut count = || {
                        columns
                            .next()
                            .and_then(|n| n.parse::<u64>().ok())
                            .unwrap_or(0)
                    };
                    added += count();
                    removed += count();
                }
                changes = Some(if added == 0 && removed == 0 {
                    "No changes".into()
                } else {
                    format!("+{added} -{removed}")
                });
                break;
            }
        }
    }
    let present = |text: String| Some(single_line(&text)).filter(|t| !t.is_empty());
    Git {
        project: root
            .and_then(|root| {
                Path::new(&root)
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
            })
            .and_then(present),
        branch: head.and_then(present),
        changes,
    }
}

/// `collect` with real git, each command bounded by two seconds.
pub async fn git(cwd: PathBuf, fields: GitFields) -> Git {
    collect(fields, |args| {
        let mut command = tokio::process::Command::new("git");
        command
            .arg("-C")
            .arg(&cwd)
            .args(args)
            .env("GIT_OPTIONAL_LOCKS", "0")
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        async move {
            let output = tokio::time::timeout(GIT_TIMEOUT, command.output())
                .await
                .ok()?
                .ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        }
    })
    .await
}

#[cfg(unix)]
pub fn hostname() -> Option<String> {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its full length, which is passed alongside it.
    let result = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) };
    if result != 0 {
        return None;
    }
    let end = buffer.iter().position(|b| *b == 0).unwrap_or(buffer.len());
    Some(String::from_utf8_lossy(&buffer[..end]).into_owned()).filter(|name| !name.is_empty())
}

#[cfg(not(unix))]
pub fn hostname() -> Option<String> {
    None
}
