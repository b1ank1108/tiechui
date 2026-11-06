//! TUI state driven by protocol events and keys; I/O lives in `lib.rs`.

use std::cmp::Ordering;
use std::collections::{HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui_core::style::{Color, Modifier, Style};
use ratatui_core::text::{Line, Span};
use tiechui_protocol::{
    ApprovalId, Event, EventKind, InputContent, InputDelivery, Item, KnownItem, ModelInfo, Op,
    QueuedInput, SessionEpoch, Submission, SubmissionId, ThinkingLevel, ThreadId, TokenUsage,
    TurnId, TurnStatus,
};

use crate::activity::{self, elapsed};
use crate::commands::{self, Action, Builtin, Command};
use crate::composer::{self, Composer, Draft};
use crate::header::Header;
use crate::history::InputHistory;
use crate::mentions::{self, Index, Match};
use crate::pager::Pager;
use crate::paste;
use crate::prompt::{self, Prompt};
use crate::resume::{self, Loaded, Resume};
use crate::select::{self, Select};
use crate::status::{self, Git, GitFields, Phase, Snapshot, StatusLineConfig};
use crate::status_picker::{Outcome, StatusPicker};
use crate::theme::Theme;
use crate::transcript::Transcript;
use crate::wrap;
use crate::{Options, Target};

/// Keys closer together than this belong to a paste from a terminal without bracketed paste.
const BURST_GAP: Duration = Duration::from_millis(8);
const MARKER: &str = "› ";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Running,
    Quit,
    Suspend,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Armed {
    Clear,
    Quit,
}

/// What to do with inputs coming back from `Op::Retract`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Backfill {
    Replace,
    Merge,
}

struct Mention {
    query: String,
    range: std::ops::Range<usize>,
    matches: Vec<Match>,
    selected: usize,
    skills: bool,
    navigated: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum IndexState {
    Idle,
    Requested,
    Loading,
}

pub struct PasteTicket {
    draft: Draft,
    revision: u64,
    side_id: Option<String>,
    side: bool,
    thread: Option<(ThreadId, SessionEpoch)>,
    generation: u64,
}

pub struct App {
    paste_generation: u64,
    theme: Theme,
    theme_name: String,
    themes_dir: Option<PathBuf>,
    theme_picker: Option<crate::theme_picker::ThemePicker>,
    save_theme: Option<String>,
    width: usize,
    height: usize,
    thread: Option<(ThreadId, SessionEpoch)>,
    turn: Option<TurnId>,
    started: Option<Instant>,
    retry_waiting: bool,
    /// The reason, when it started and the latest progress step.
    compaction: Option<(tiechui_protocol::CompactionReason, Instant, String)>,
    retry_countdown: Option<(u32, Option<u32>, Instant)>,
    retry_available: bool,
    paused: Option<Instant>,
    /// Animation phase origin.
    epoch: Instant,
    transcript: Transcript,
    fullscreen: bool,
    main_view: Pager,
    main_body_height: usize,
    /// The last mouse position, for the control row under it.
    pointer: Option<(usize, usize)>,
    pager: Option<Pager>,
    selection: crate::selection::Selection,
    copy_on_select: bool,
    copy_hint: Option<(Span<'static>, Instant)>,
    context_panel: Option<crate::context::ContextPanel>,
    side: Option<crate::btw::Panel>,
    side_visible: bool,
    agents: Option<crate::subagents::Panel>,
    agents_visible: bool,
    shells: Option<crate::shells::Panel>,
    shells_visible: bool,
    shell_indicator: Option<String>,
    permission: crate::status::PermissionMode,
    /// Expanded transcript rows for the first `.0` cells at width `.1`.
    expanded: (usize, usize, Vec<Line<'static>>),
    outbox: Vec<(SubmissionId, Op)>,
    next_id: u64,
    startup: Vec<Op>,
    cwd: PathBuf,
    queue_hint: &'static str,
    composer: Composer,
    focused: bool,
    refilled: bool,
    saved_refill: bool,
    steering: Vec<QueuedInput>,
    follow_up: Vec<QueuedInput>,
    /// Drafts of inputs sent while a turn runs, restored exactly on retraction. The flag
    /// records whether the input has been seen in a queue, so consumed ones can be dropped.
    drafts: HashMap<SubmissionId, (Draft, bool)>,
    backfills: VecDeque<Backfill>,
    interrupting: bool,
    armed: Option<Armed>,
    notice: Option<String>,
    stash: Option<Draft>,
    inputs: InputHistory,
    browsing: Option<(usize, u64)>,
    help: bool,
    jump: Option<bool>,
    burst: Option<(Instant, usize)>,
    mention: Option<Mention>,
    dismissed: Option<(usize, String)>,
    index: Option<Index>,
    index_state: IndexState,
    resources: Vec<tiechui_protocol::Resource>,
    resource_picker: Option<(Draft, tiechui_protocol::ResourceKind)>,
    search_mode: mentions::Mode,
    index_root: Option<PathBuf>,
    index_generation: u64,
    index_cancel: tokio_util::sync::CancellationToken,
    session: Session,
    home: Option<PathBuf>,
    hostname: Option<String>,
    goal_status: Option<String>,
    loop_status: Option<String>,
    plugin_status: std::collections::BTreeMap<String, String>,
    input_blocked: bool,
    status_line: StatusLineConfig,
    picker: Option<StatusPicker>,
    select: Option<Select>,
    /// The session's open request; activity is paused while it awaits an answer.
    prompt: Option<(ApprovalId, Prompt)>,
    models: Vec<ModelInfo>,
    save: Option<StatusLineConfig>,
    git: Git,
    /// The fields `git` holds, or `None` once stale; one request runs at a time.
    git_fields: Option<GitFields>,
    git_loading: bool,
    commands: Vec<Command>,
    menu: Option<Menu>,
    /// The draft whose menu Esc closed; it stays closed until the draft changes.
    menu_dismissed: Option<String>,
    trust: Option<String>,
    quit: bool,
    resume: Option<Resume>,
    /// Counts resume pickers, so reads for a closed one are dropped.
    generation: u64,
    switch: Option<Target>,
    /// Set from leaving one session until the next one's thread starts.
    fresh: bool,
    /// What to say once the next session opens.
    arrival: Option<&'static str>,
    banner: bool,
    /// The next session to open prints the header.
    banner_due: bool,
}

struct Menu {
    matches: Vec<usize>,
    selected: usize,
}

/// What the session reports about itself, for the status line.
#[derive(Default)]
struct Session {
    model: Option<ModelInfo>,
    thinking: Option<ThinkingLevel>,
    context: Option<TokenUsage>,
    total: TokenUsage,
    last: Option<TurnStatus>,
    /// A checkpoint replaced the context and no request has measured it since.
    compacted: bool,
}

/// Terminals where Alt+↑ is usually taken advertise Shift+← instead; both always work.
pub fn queue_hint(env: &dyn Fn(&str) -> Option<String>) -> &'static str {
    let program = env("TERM_PROGRAM").unwrap_or_default();
    if env("TMUX").is_some() || ["Apple_Terminal", "WarpTerminal", "vscode"].contains(&&*program) {
        "shift+left"
    } else {
        "alt+up"
    }
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

fn parts(content: &[InputContent]) -> (String, Vec<(String, String)>) {
    let mut texts = Vec::new();
    let mut images = Vec::new();
    for part in content {
        match part {
            InputContent::Text { text } => texts.push(text.as_str()),
            InputContent::Image { mime_type, data } => {
                images.push((mime_type.clone(), data.clone()))
            }
        }
    }
    (texts.join("\n"), images)
}

const SHORTCUTS: [(&str, &str); 12] = [
    (
        "enter",
        "send; while working, steer after the next tool call",
    ),
    ("tab", "queue a follow-up while working"),
    ("shift+enter", "new line (also ctrl+j)"),
    ("esc", "interrupt; twice to clear the draft"),
    ("", "edit the last queued follow-up"),
    ("alt+s", "stash or restore the draft"),
    ("↑ / ↓", "browse input history"),
    ("@", "mention a file"),
    ("ctrl+-", "undo"),
    ("ctrl+c", "clear; twice to quit"),
    ("ctrl+d", "quit when empty"),
    ("ctrl+z", "suspend"),
];

impl App {
    pub fn new(mut theme: Theme, width: usize, options: Options) -> Self {
        let theme_name = options
            .syntax_theme
            .clone()
            .unwrap_or_else(|| "automatic".into());
        if let Some(entry) = crate::theme_picker::catalog(options.themes_dir.as_deref())
            .into_iter()
            .find(|entry| entry.name == theme_name)
        {
            theme.syntax = entry.theme;
        }
        let (inputs, warning) = options.history.map(InputHistory::load).unwrap_or_default();
        let transcript = Transcript::new(theme.clone(), width);
        Self {
            theme,
            theme_name,
            themes_dir: options.themes_dir,
            theme_picker: None,
            save_theme: None,
            width: width.max(1),
            height: 24,
            thread: None,
            turn: None,
            started: None,
            retry_waiting: false,
            compaction: None,
            retry_countdown: None,
            retry_available: false,
            paused: None,
            epoch: Instant::now(),
            transcript,
            fullscreen: !options.no_alt_screen,
            main_view: Pager::default(),
            main_body_height: 0,
            pointer: None,
            pager: None,
            selection: crate::selection::Selection::default(),
            copy_on_select: options.copy_on_select,
            copy_hint: None,
            context_panel: None,
            side: None,
            side_visible: false,
            agents: None,
            agents_visible: false,
            shells: None,
            shells_visible: false,
            shell_indicator: None,
            permission: Default::default(),
            expanded: (0, 0, Vec::new()),
            outbox: Vec::new(),
            next_id: 0,
            startup: options.startup,
            cwd: options.cwd,
            queue_hint: options.queue_hint.unwrap_or("alt+up"),
            composer: Composer::default(),
            focused: true,
            refilled: false,
            saved_refill: false,
            steering: Vec::new(),
            follow_up: Vec::new(),
            drafts: HashMap::new(),
            backfills: VecDeque::new(),
            interrupting: false,
            armed: None,
            notice: warning,
            stash: None,
            inputs,
            browsing: None,
            help: false,
            jump: None,
            burst: None,
            mention: None,
            dismissed: None,
            index: None,
            paste_generation: 0,
            index_state: IndexState::Idle,
            resources: vec![],
            resource_picker: None,
            search_mode: mentions::Mode::default(),
            index_root: None,
            index_generation: 0,
            index_cancel: tokio_util::sync::CancellationToken::new(),
            session: Session::default(),
            home: options.home.clone(),
            hostname: options.hostname,
            goal_status: None,
            loop_status: None,
            plugin_status: Default::default(),
            input_blocked: false,
            status_line: options.status_line,
            picker: None,
            select: None,
            prompt: None,
            models: Vec::new(),
            save: None,
            git: Git::default(),
            git_fields: None,
            git_loading: false,
            commands: commands::catalog(&[]),
            menu: None,
            menu_dismissed: None,
            trust: None,
            quit: false,
            resume: options
                .pick
                .then(|| Resume::new(1, true, options.home.clone(), now_ms())),
            generation: 1,
            switch: None,
            fresh: false,
            arrival: None,
            banner: options.banner,
            banner_due: options.banner,
        }
    }

    fn print_header(&mut self) {
        if !self.transcript.header_pending() {
            return;
        }
        let model = self.session.model.as_ref();
        self.transcript.header(Header {
            model: model.map(|model| model.id.clone()),
            thinking: model
                .filter(|model| model.reasoning)
                .and(self.session.thinking)
                .map(|level| status::thinking_name(level).to_owned()),
            cwd: status::compact_path(&self.cwd, self.home.as_deref()),
        });
    }

    pub fn thread(&self) -> Option<&(ThreadId, SessionEpoch)> {
        self.thread.as_ref()
    }

    pub fn running(&self) -> bool {
        self.turn.is_some()
    }

    pub fn take_submissions(&mut self) -> Vec<Submission> {
        let Some((thread_id, epoch)) = self.thread.clone().filter(|_| !self.fresh) else {
            return Vec::new();
        };
        let blocked = !self.composer.is_empty()
            || self.resume.is_some()
            || self.select.is_some()
            || self.picker.is_some()
            || self.theme_picker.is_some()
            || self.prompt.is_some();
        // Drafts hold back module work that starts turns on its own.
        if (self.goal_status.is_some()
            || self.loop_status.is_some()
            || self.shells.as_ref().is_some_and(|panel| !panel.is_empty()))
            && blocked != self.input_blocked
        {
            self.input_blocked = blocked;
            self.send(Op::InputState { blocked });
        }
        std::mem::take(&mut self.outbox)
            .into_iter()
            .map(|(id, op)| Submission {
                id,
                thread_id: thread_id.clone(),
                epoch,
                op,
            })
            .collect()
    }

    pub fn take_history(&mut self) -> Vec<Line<'static>> {
        self.transcript.take_history()
    }

    pub fn take_theme_save(&mut self) -> Option<String> {
        self.save_theme.take()
    }

    pub fn reload_theme(&mut self, name: Option<String>) {
        self.theme_name = name.unwrap_or_else(|| "automatic".into());
        let entries = crate::theme_picker::catalog(self.themes_dir.as_deref());
        self.theme.syntax = entries
            .into_iter()
            .find(|entry| entry.name == self.theme_name)
            .and_then(|entry| entry.theme);
        self.theme_changed();
    }

    fn theme_changed(&mut self) {
        self.transcript.set_theme(self.theme.clone());
        self.expanded = (0, 0, Vec::new());
    }

    pub fn colors(
        &mut self,
        foreground: Option<crate::palette::Rgb>,
        background: Option<crate::palette::Rgb>,
    ) {
        let palette = crate::palette::Palette {
            foreground: foreground.or(self.theme.palette.foreground),
            background: background.or(self.theme.palette.background),
            ..self.theme.palette
        };
        if palette != self.theme.palette {
            let syntax = self.theme.syntax.clone();
            self.theme = Theme::new(palette);
            self.theme.syntax = syntax;
            self.theme_changed();
            self.transcript.schedule_reflow(Instant::now());
        }
    }

    /// The working directory to index once, the first time `@` is typed.
    pub fn take_index_request(
        &mut self,
    ) -> Option<(PathBuf, u64, tokio_util::sync::CancellationToken)> {
        (self.index_state == IndexState::Requested).then(|| {
            self.index_state = IndexState::Loading;
            (
                self.index_root.clone().unwrap_or_else(|| self.cwd.clone()),
                self.index_generation,
                self.index_cancel.clone(),
            )
        })
    }

    /// The git facts the visible items need, when they are missing or stale.
    pub fn take_git_request(&mut self) -> Option<(PathBuf, GitFields)> {
        let mut items = self.status_line.items.clone();
        if let Some(picker) = &self.picker {
            items.extend(picker.selection().items);
        }
        let fields = GitFields::of(&items);
        if self.git_loading || self.git_fields == Some(fields) {
            return None;
        }
        if !fields.any() {
            self.git_fields = Some(fields);
            return None;
        }
        self.git_loading = true;
        Some((self.cwd.clone(), fields))
    }

    pub fn set_git(&mut self, fields: GitFields, git: Git) {
        self.git_loading = false;
        self.git = git;
        self.git_fields = Some(fields);
    }

    /// A `/statusline` choice to persist.
    pub fn take_save(&mut self) -> Option<StatusLineConfig> {
        self.save.take()
    }

    pub fn set_notice(&mut self, notice: String) {
        self.notice = Some(notice);
    }

    /// `/trust` arguments to apply to the project's trust decision.
    pub fn take_trust(&mut self) -> Option<String> {
        self.trust.take()
    }

    /// Where the current session works.
    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    /// The session `/new`, `/resume` or `/reload` asked for, and the epoch it starts at. The
    /// current one is left at once: the host ends it before opening the next.
    pub fn take_switch(&mut self) -> Option<(Target, SessionEpoch)> {
        let target = self.switch.take()?;
        self.release_index();
        self.mention = None;
        self.resources.clear();
        self.close_resource_picker();
        self.paste_generation += 1;
        let epoch = SessionEpoch(self.thread.as_ref().map_or(0, |(_, epoch)| epoch.0 + 1));
        self.arrival = Some(match target {
            Target::New => "New session started",
            Target::Resume(_) => "Resumed session",
            Target::Reload => "Reloaded",
        });
        self.selection.clear();
        self.copy_hint = None;
        if target != Target::Reload {
            self.transcript.reset();
            self.main_view = Pager::default();
            self.expanded = (0, 0, Vec::new());
            self.banner_due = self.banner;
        }
        self.goal_status = None;
        self.loop_status = None;
        self.plugin_status.clear();
        self.input_blocked = false;
        self.turn = None;
        self.started = None;
        self.paused = None;
        self.retry_waiting = false;
        self.retry_countdown = None;
        self.retry_available = false;
        self.outbox.clear();
        self.steering.clear();
        self.follow_up.clear();
        self.drafts.clear();
        self.backfills.clear();
        self.interrupting = false;
        self.prompt = None;
        self.select = None;
        self.pager = None;
        self.agents = None;
        self.agents_visible = false;
        self.shells = None;
        self.shells_visible = false;
        self.shell_indicator = None;
        self.context_panel = None;
        self.side = None;
        self.side_visible = false;
        self.menu = None;
        self.session = Session::default();
        self.models.clear();
        self.commands = commands::catalog(&[]);
        self.fresh = target != Target::Reload;
        Some((target, epoch))
    }

    /// The host opened the next session; `warnings` are what preparing it reported.
    pub fn opened(
        &mut self,
        cwd: PathBuf,
        status_line: StatusLineConfig,
        copy_on_select: bool,
        warnings: Vec<String>,
    ) {
        self.selection.clear();
        self.copy_hint = None;
        self.copy_on_select = copy_on_select;
        self.status_line = status_line;
        self.cwd = cwd;
        self.paste_generation += 1;
        self.release_index();
        self.mention = None;
        self.resources.clear();
        self.close_resource_picker();
        self.git_fields = None;
        if !warnings.is_empty() {
            self.transcript.info("session".into(), warnings.join("\n"));
        }
        self.notice = self.arrival.take().map(str::to_owned);
    }

    /// Reads the resume picker is waiting for.
    pub fn take_reads(&mut self) -> Option<resume::Reads> {
        let resume = self.resume.as_mut()?;
        let reads = resume.take_reads();
        (!reads.is_empty()).then(|| resume::Reads {
            generation: resume.generation,
            cancel: resume.cancel(),
            reads,
        })
    }

    /// A read's result, dropped unless the picker that asked is still open.
    pub fn loaded(&mut self, generation: u64, loaded: Loaded) {
        if let Some(resume) = &mut self.resume
            && resume.generation == generation
        {
            resume.loaded(loaded);
        }
    }

    /// A report from work done outside the session, such as `/trust`.
    pub fn command_output(&mut self, name: &str, text: String) {
        let title = if name == tiechui_protocol::BTW_SHARE {
            "Shared side answer".into()
        } else {
            format!("/{name}")
        };
        self.transcript.info(title, text);
    }

    fn release_index(&mut self) {
        self.index_cancel.cancel();
        self.index_cancel = tokio_util::sync::CancellationToken::new();
        self.index_generation += 1;
        self.index = None;
        self.index_root = None;
        self.index_state = IndexState::Idle;
    }

    pub fn set_index_result(&mut self, generation: u64, index: Index) {
        if generation == self.index_generation && !self.index_cancel.is_cancelled() {
            self.set_index(index);
        }
    }

    pub fn set_index(&mut self, index: Index) {
        self.index = Some(index);
        self.refresh_mention();
    }

    pub fn resize(&mut self, width: usize, height: usize) {
        self.selection.clear();
        if self.width != width.max(1) || self.height != height.max(1) {
            self.transcript.schedule_reflow(Instant::now());
        }
        self.width = width.max(1);
        self.height = height.max(1);
        self.transcript.resize(self.width);
    }

    pub fn reflow_pending(&self) -> bool {
        self.transcript.reflow_pending()
    }

    pub fn reflow(&mut self, now: Instant, cap: usize) -> Option<Vec<Line<'static>>> {
        self.transcript.reflow(now, cap)
    }

    fn send(&mut self, op: Op) -> SubmissionId {
        self.next_id += 1;
        let id = SubmissionId(format!("tui-{}", self.next_id));
        self.outbox.push((id.clone(), op));
        id
    }

    pub fn event(&mut self, event: Event) {
        self.event_at(event, Instant::now());
    }

    pub fn event_at(&mut self, event: Event, now: Instant) {
        if let Some((thread, epoch)) = &self.thread {
            let stale = match event.epoch.0.cmp(&epoch.0) {
                Ordering::Less => true,
                Ordering::Equal => event.thread_id != *thread,
                // A later epoch opens with its thread, which may be another session's.
                Ordering::Greater => !matches!(event.event, EventKind::ThreadStarted { .. }),
            };
            if stale {
                return;
            }
        }
        // The session reports its model right after the thread starts; anything else first
        // releases the header with what is known.
        if self.transcript.header_pending()
            && !matches!(
                event.event,
                EventKind::SessionUpdated { .. } | EventKind::ComposerRestored { .. }
            )
        {
            self.print_header();
        }
        match event.event {
            EventKind::ThreadStarted { thread, replay } => {
                self.side = None;
                self.side_visible = false;
                self.agents = None;
                self.agents_visible = false;
                self.shells = None;
                self.shells_visible = false;
                self.shell_indicator = None;
                self.composer.reset_escape();
                let first = std::mem::take(&mut self.fresh) | self.thread.is_none();
                self.thread = Some((thread.id, thread.epoch));
                if first && std::mem::take(&mut self.banner_due) {
                    self.transcript.hold_header();
                }
                if first || replay {
                    self.transcript.replay(&thread.turns, now);
                }
                self.session.compacted = thread
                    .turns
                    .iter()
                    .flat_map(|turn| &turn.items)
                    .rev()
                    .find_map(|item| match item {
                        Item::Known(item) => Some(item),
                        _ => None,
                    })
                    .is_some_and(|item| matches!(item, KnownItem::Compaction { .. }));
                if first {
                    for op in std::mem::take(&mut self.startup) {
                        self.send(op);
                    }
                }
            }
            EventKind::TurnStarted { turn } => {
                self.composer.reset_escape();
                self.compaction = None;
                self.retry_waiting = false;
                self.retry_countdown = None;
                self.notice = None;
                self.turn = Some(turn.id);
                self.started = Some(now);
                self.paused = None;
            }
            EventKind::ItemStarted {
                item: Item::Known(item),
                ..
            } => self.transcript.started(item, now),
            EventKind::ItemDelta { item_id, delta, .. } => self.transcript.delta(&item_id, &delta),
            EventKind::ItemCompleted {
                item: Item::Known(item),
                ..
            } => {
                if matches!(item, KnownItem::Compaction { .. }) {
                    self.session.compacted = true;
                }
                self.transcript.completed(item, now);
            }
            EventKind::TurnCompleted { turn } => {
                self.release_index();
                self.retry_waiting = false;
                self.retry_countdown = None;
                if self.retry_available {
                    self.notice = Some("/retry retry the failed request".into());
                }
                self.close_prompt(now);
                let worked = self.started.take().map(|started| now - started);
                self.turn = None;
                self.interrupting = false;
                self.session.last = Some(turn.status);
                // Tools may have committed or switched branches.
                if self.git_fields.is_some_and(GitFields::any) {
                    self.git_fields = None;
                }
                self.transcript.turn_completed(turn.status, worked);
                self.refresh_mention();
            }
            EventKind::CompactionStarted { reason } => {
                self.compaction = Some((reason, now, String::new()));
                self.retry_waiting = false;
                self.retry_countdown = None;
                self.resume_activity(now);
            }
            EventKind::CompactionProgress {
                reason,
                phase,
                retries,
                shrinks,
            } => {
                let mut step = vec![
                    match phase {
                        tiechui_protocol::CompactionPhase::Prepare => "preparing",
                        tiechui_protocol::CompactionPhase::Summarize => "summarizing",
                        tiechui_protocol::CompactionPhase::Restore => "restoring context",
                        tiechui_protocol::CompactionPhase::Commit => "saving",
                    }
                    .to_owned(),
                ];
                if retries > 0 {
                    step.push(format!("retry {retries}"));
                }
                if shrinks > 0 {
                    step.push(format!("trimmed {shrinks}×"));
                }
                let started = self.compaction.as_ref().map_or(now, |(_, at, _)| *at);
                self.compaction = Some((reason, started, step.join(" · ")));
            }
            EventKind::CompactionEnded {
                reason,
                aborted,
                error,
                paused,
                ..
            } => {
                self.compaction = None;
                self.retry_waiting = false;
                self.retry_countdown = None;
                self.resume_activity(now);
                if aborted {
                    self.transcript.error(
                        if reason == tiechui_protocol::CompactionReason::Manual {
                            "Compaction cancelled"
                        } else {
                            "Auto-compaction cancelled"
                        }
                        .into(),
                    );
                } else if let Some(error) = error {
                    self.transcript.error(error);
                }
                if let Some(paused) = paused {
                    self.transcript.warning(paused);
                }
            }
            EventKind::RetryAvailable { available } => {
                self.retry_available = available;
                if available && !self.running() {
                    self.notice = Some("/retry retry the failed request".into());
                }
            }
            EventKind::RequestRetry {
                attempt,
                max_retries,
                delay_ms,
                ..
            } => {
                self.retry_waiting = delay_ms.is_some();
                self.retry_countdown = delay_ms
                    .map(|delay| (attempt, max_retries, now + Duration::from_millis(delay)));
                if self.retry_waiting {
                    self.paused.get_or_insert(now);
                } else {
                    self.resume_activity(now);
                }
            }
            EventKind::SessionUpdated {
                model,
                thinking,
                context,
                total,
            } => {
                self.composer.reset_escape();
                self.session.model = Some(model);
                self.session.thinking = Some(thinking);
                self.session.compacted &= context.is_none();
                self.session.context = context;
                self.session.total = total;
                self.print_header();
            }
            EventKind::TokenUsageUpdated { last, total, .. } => {
                self.session.compacted = false;
                self.session.context = Some(last);
                self.session.total = total;
            }
            EventKind::ApprovalRequested {
                approval_id,
                request,
                ..
            } => {
                self.composer.reset_escape();
                if let tiechui_protocol::ApprovalRequest::Rewind { panel } = &request
                    && let Some((id, Prompt::Rewind(prompt))) = &mut self.prompt
                    && *id == approval_id
                {
                    prompt.update(panel.clone());
                    return;
                }
                self.close_prompt(now);
                self.prompt = Some((approval_id, Prompt::new(request)));
                self.paused.get_or_insert(now);
            }
            EventKind::ApprovalResolved { approval_id }
                if self
                    .prompt
                    .as_ref()
                    .is_some_and(|(id, ..)| *id == approval_id) =>
            {
                self.close_prompt(now)
            }
            EventKind::QueueUpdated {
                steering,
                follow_up,
            } => {
                let live: HashSet<&SubmissionId> = steering
                    .iter()
                    .chain(&follow_up)
                    .map(|input| &input.submission_id)
                    .collect();
                self.drafts.retain(|id, (_, seen)| {
                    if live.contains(id) {
                        *seen = true;
                    }
                    live.contains(id) || !*seen
                });
                self.steering = steering;
                self.follow_up = follow_up;
            }
            EventKind::InputRetracted { inputs } => self.backfill(&inputs),
            EventKind::ModelsUpdated { models } => self.models = models,
            EventKind::ResourcesUpdated { resources } => {
                self.resources = resources;
                self.refresh_mention();
            }
            EventKind::CommandsUpdated { commands } => {
                let chosen = self
                    .menu
                    .as_ref()
                    .and_then(|menu| menu.matches.get(menu.selected))
                    .map(|index| self.commands[*index].info.name.clone());
                self.commands = commands::catalog(&commands);
                self.refresh_menu();
                if let (Some(name), Some(menu)) = (chosen, &mut self.menu)
                    && let Some(at) = menu
                        .matches
                        .iter()
                        .position(|index| self.commands[*index].info.name == name)
                {
                    menu.selected = at;
                }
            }
            EventKind::ComposerRestored { content } if self.composer.is_empty() => {
                self.refill(content)
            }
            EventKind::ComposerRefilled { content } => self.refill(content),
            EventKind::DraftImagesDetached => {
                self.composer.detach_images();
                self.saved_refill = false;
            }
            EventKind::CommandOutput { name, text } if !text.is_empty() => {
                self.command_output(&name, text)
            }
            EventKind::CommandPanel { panel, view, .. } => {
                self.composer.reset_escape();
                self.context_panel = Some(crate::context::ContextPanel::new(*panel, view));
            }
            EventKind::Extension { name, data }
                if name == tiechui_protocol::RESOURCE_PICKER_EVENT =>
            {
                if let Ok(request) =
                    serde_json::from_value::<tiechui_protocol::ResourcePickerRequest>(data)
                {
                    if self.resource_picker.is_none() {
                        self.resource_picker = Some((self.composer.draft(), request.kind));
                    }
                    self.composer.set_text(
                        if request.kind == tiechui_protocol::ResourceKind::Skill {
                            "$"
                        } else {
                            "@"
                        },
                    );
                    self.menu = None;
                    self.dismissed = None;
                    self.refresh_mention();
                }
            }
            EventKind::Extension { name, data } if name == tiechui_protocol::PLUGIN_EVENT => {
                if let Ok(status) = serde_json::from_value::<tiechui_protocol::PluginStatus>(data) {
                    if status.text.is_empty() {
                        self.plugin_status.remove(&status.plugin);
                    } else {
                        self.plugin_status.insert(status.plugin, status.text);
                    }
                }
            }
            EventKind::Extension { name, data } if name == "tiechui.goal.status" => {
                self.goal_status = data.as_str().map(str::to_owned);
            }
            EventKind::Extension { name, data } if name == "tiechui.loop.status" => {
                self.loop_status = data.as_str().map(str::to_owned);
            }
            EventKind::Extension { name, data }
                if name == "module.notice" || name == tiechui_protocol::PLUGIN_NOTICE_EVENT =>
            {
                self.notice = data.as_str().map(str::to_owned);
            }
            EventKind::Extension { name, data } if name == tiechui_protocol::SUBAGENTS_EVENT => {
                if let Ok(view) = serde_json::from_value::<tiechui_protocol::SubagentsView>(data) {
                    if let Some(panel) = &mut self.agents {
                        panel.update(view.records);
                    } else {
                        self.agents = Some(crate::subagents::Panel::new(view.records));
                    }
                    if view.open && !self.context_open() && self.prompt.is_none() {
                        self.agents_visible = true;
                    }
                }
            }
            EventKind::Extension { name, data } if name == tiechui_protocol::SHELLS_EVENT => {
                if let Ok(view) = serde_json::from_value::<tiechui_protocol::ShellsView>(data) {
                    self.shell_indicator = crate::shells::indicator(&view);
                    let open = view.open;
                    match &mut self.shells {
                        Some(panel) => panel.update(view),
                        None => self.shells = Some(crate::shells::Panel::new(view)),
                    }
                    if open && !self.context_open() && self.prompt.is_none() {
                        self.shells_visible = true;
                    }
                }
            }
            EventKind::Extension { name, data }
                if name == tiechui_protocol::PERMISSION_MODE_EVENT =>
            {
                if let Ok(mode) = serde_json::from_value(data) {
                    self.permission = mode;
                }
            }
            EventKind::Extension { name, data } if name == tiechui_protocol::BTW_EVENT => {
                if let Ok(event) = serde_json::from_value::<tiechui_protocol::BtwEvent>(data) {
                    match event {
                        tiechui_protocol::BtwEvent::Closed => {
                            self.side = None;
                            self.side_visible = false;
                        }
                        tiechui_protocol::BtwEvent::Update { view, open } => {
                            if let Some(side) = &mut self.side {
                                if side.view.id == view.id {
                                    side.update(view);
                                } else if open {
                                    self.side = Some(crate::btw::Panel::new(view));
                                }
                            } else {
                                self.side = Some(crate::btw::Panel::new(view));
                            }
                            if open && !self.context_open() && self.prompt.is_none() {
                                self.side_visible = true;
                            }
                        }
                        tiechui_protocol::BtwEvent::Models { models, current } => {
                            let mut choices = select::models(&models, current.as_ref());
                            for choice in &mut choices {
                                choice.op = Op::RunCommand {
                                    name: "btw".into(),
                                    arguments: format!("model {}", choice.label),
                                };
                            }
                            if let Err(select) =
                                Select::open("Side conversation model", choices, "")
                            {
                                self.select = Some(select);
                            }
                        }
                    }
                }
            }
            EventKind::Error { message } => self.transcript.error(message),
            _ => {}
        }
    }

    fn backfill(&mut self, inputs: &[QueuedInput]) {
        let mode = self.backfills.pop_front().unwrap_or(Backfill::Merge);
        let mut drafts: Vec<Draft> = inputs
            .iter()
            .map(|input| {
                self.drafts.remove(&input.submission_id).map_or_else(
                    || {
                        let (text, images) = parts(&input.content);
                        Draft::from_parts(&text, images)
                    },
                    |(draft, _)| draft,
                )
            })
            .collect();
        if drafts.is_empty() {
            return;
        }
        if mode == Backfill::Merge {
            drafts.push(self.composer.draft());
        }
        self.composer.restore(Draft::join(drafts));
        self.browsing = None;
        self.refresh_mention();
    }

    fn submit(&mut self, delivery: InputDelivery) {
        self.main_view.follow();
        if self.thread.is_none() {
            return;
        }
        let (text, images) = if self.refilled {
            self.composer.submission_without_image_labels()
        } else {
            self.composer.submission()
        };
        if text.trim().is_empty() && images.is_empty() {
            return;
        }
        if (images.is_empty() || (self.refilled && text.trim() == "/rewind detach"))
            && let Some((command, arguments)) = commands::parse(&self.commands, &text)
        {
            let (name, action, arguments) = (
                command.info.name.clone(),
                command.action.clone(),
                arguments.to_owned(),
            );
            self.remember(&text);
            self.composer.clear();
            self.clear_saved_refill();
            self.refilled = false;
            self.run_command(name, action, arguments);
            return;
        }
        if images.is_empty() && text.trim_start().starts_with('!') {
            if self.running() {
                self.notice = Some("Shell commands run after the current turn".into());
                return;
            }
            let command = text.trim_start()[1..].trim().to_owned();
            if command.is_empty() {
                return;
            }
            self.remember(&text);
            self.composer.clear();
            self.send(Op::RunShell { command });
            return;
        }
        self.refilled = false;
        self.saved_refill = false;
        let draft = self.composer.clear();
        self.remember(&text);
        let mut content = Vec::new();
        if !text.is_empty() {
            content.push(InputContent::Text { text });
        }
        content.extend(
            images
                .into_iter()
                .map(|(mime_type, data)| InputContent::Image { mime_type, data }),
        );
        let running = self.running();
        let id = self.send(Op::UserInput { content, delivery });
        if running {
            self.drafts.insert(id, (draft, false));
        }
        if let Some(stash) = self.stash.take() {
            self.composer.restore(stash);
        }
    }

    fn remember(&mut self, text: &str) {
        let session = self
            .thread
            .as_ref()
            .map(|(id, _)| id.0.clone())
            .unwrap_or_default();
        if let Some(warning) = self.inputs.remember(text, &session) {
            self.notice = Some(warning);
        }
        self.browsing = None;
    }

    /// `/new`: a new session that keeps the model and thinking level.
    fn new_session(&mut self) {
        if let Some(model) = &self.session.model {
            self.startup = vec![Op::SetModel {
                provider: model.provider.clone(),
                model: format!("{}/{}", model.provider, model.id),
            }];
            self.startup
                .extend(self.session.thinking.map(|level| Op::SetThinking { level }));
        }
        self.switch = Some(Target::New);
    }

    fn run_command(&mut self, name: String, action: Action, arguments: String) {
        match action {
            Action::Builtin(Builtin::Compact) => {
                self.send(Op::Compact {
                    instructions: (!arguments.is_empty()).then_some(arguments),
                });
            }
            Action::Builtin(Builtin::Retry) => {
                if !arguments.is_empty() {
                    self.notice = Some("Usage: /retry".into());
                } else if self.prompt.is_some() {
                    self.notice = Some("Finish the current interaction before retrying.".into());
                } else {
                    self.send(Op::Retry);
                }
            }
            Action::Builtin(Builtin::Model) => {
                let choices = select::models(&self.models, self.session.model.as_ref());
                self.choose("Select Model", choices, &arguments);
            }
            Action::Builtin(Builtin::Thinking) => {
                let levels = self
                    .session
                    .model
                    .as_ref()
                    .map_or_else(|| vec![ThinkingLevel::Off], |m| m.thinking_levels.clone());
                let choices = select::thinking(&levels, self.session.thinking);
                self.choose("Thinking Level", choices, &arguments);
            }
            Action::Builtin(Builtin::Statusline) => {
                self.picker = Some(StatusPicker::new(&self.status_line));
            }
            Action::Builtin(Builtin::Theme) => {
                self.theme_picker = Some(crate::theme_picker::ThemePicker::new(
                    crate::theme_picker::catalog(self.themes_dir.as_deref()),
                    &self.theme_name,
                    &self.theme,
                ));
            }
            Action::Builtin(Builtin::Trust) => self.trust = Some(arguments),
            Action::Builtin(Builtin::New) => self.new_session(),
            Action::Builtin(Builtin::Reload) => self.switch = Some(Target::Reload),
            Action::Builtin(Builtin::Resume) => {
                self.generation += 1;
                self.resume = Some(Resume::new(
                    self.generation,
                    false,
                    self.home.clone(),
                    now_ms(),
                ));
            }
            Action::Builtin(Builtin::Quit) => self.quit = true,
            Action::Builtin(Builtin::Help) => {
                let width = self
                    .commands
                    .iter()
                    .map(|c| wrap::width(&c.info.name) + 1)
                    .max()
                    .unwrap_or(0);
                let mut text: Vec<String> = self
                    .commands
                    .iter()
                    .map(|c| {
                        format!(
                            "{:<width$}  {}",
                            format!("/{}", c.info.name),
                            c.info.description
                        )
                    })
                    .collect();
                text.push("? on an empty draft shows the keyboard shortcuts".into());
                self.transcript.info("Commands".into(), text.join("\n"));
            }
            Action::Module => {
                self.send(Op::RunCommand { name, arguments });
            }
        }
    }

    fn close_prompt(&mut self, now: Instant) {
        self.prompt = None;
        self.resume_activity(now);
    }

    fn resume_activity(&mut self, now: Instant) {
        if self.prompt.is_none()
            && !self.retry_waiting
            && let Some(paused) = self.paused.take()
            && let Some(started) = &mut self.started
        {
            *started += now - paused;
        }
    }

    fn choose(&mut self, title: &'static str, choices: Vec<select::Choice>, query: &str) {
        match Select::open(title, choices, query) {
            Ok(op) => {
                self.send(op);
            }
            Err(select) => self.select = Some(select),
        }
    }

    /// The `/` menu, while the draft is a single `/word`.
    fn refresh_menu(&mut self) {
        let text = self.composer.text();
        let query = text
            .trim_start()
            .strip_prefix('/')
            .filter(|_| self.composer.cursor() > text.len() - text.trim_start().len())
            .filter(|rest| !rest.contains(char::is_whitespace) && !rest.contains('/'));
        let Some(query) = query else {
            self.menu = None;
            self.menu_dismissed = None;
            return;
        };
        if self.menu_dismissed.as_deref() == Some(text) {
            self.menu = None;
            return;
        }
        self.menu_dismissed = None;
        let matches = commands::search(&self.commands, query);
        let selected = self
            .menu
            .as_ref()
            .map_or(0, |menu| menu.selected)
            .min(matches.len().saturating_sub(1));
        self.menu = Some(Menu { matches, selected });
    }

    /// Menu keys come before queueing: Tab completes, Enter runs the selection.
    fn menu_key(&mut self, key: &KeyEvent) -> Option<Exit> {
        let menu = self.menu.as_mut()?;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let count = menu.matches.len().max(1);
        let chosen = menu
            .matches
            .get(menu.selected)
            .map(|index| self.commands[*index].info.name.clone());
        match key.code {
            KeyCode::Up => menu.selected = (menu.selected + count - 1) % count,
            KeyCode::Char('p') if control => menu.selected = (menu.selected + count - 1) % count,
            KeyCode::Down => menu.selected = (menu.selected + 1) % count,
            KeyCode::Char('n') if control => menu.selected = (menu.selected + 1) % count,
            KeyCode::Tab if key.modifiers.is_empty() => {
                if let Some(name) = chosen {
                    self.notice = self
                        .commands
                        .iter()
                        .find(|command| command.info.name == name)
                        .and_then(|command| command.info.argument_hint.as_ref())
                        .map(|hint| format!("/{name} {hint}"));
                    self.composer.set_text(&format!("/{name} "));
                }
                self.menu = None;
            }
            KeyCode::Enter if key.modifiers.is_empty() => {
                let name = chosen?;
                self.composer.set_text(&format!("/{name}"));
                self.menu = None;
                self.submit(InputDelivery::FollowUp);
            }
            KeyCode::Esc => {
                self.menu_dismissed = Some(self.composer.text().to_owned());
                self.menu = None;
            }
            _ => return None,
        }
        Some(if std::mem::take(&mut self.quit) {
            Exit::Quit
        } else {
            Exit::Running
        })
    }

    fn interrupt(&mut self) {
        if self.goal_status.is_some() {
            self.send(Op::InterruptIdle);
        }
        if let Some(turn_id) = self.turn.clone() {
            self.send(Op::Interrupt { turn_id });
            self.interrupting = true;
        }
    }

    fn retract(&mut self, submission_ids: Vec<SubmissionId>, mode: Backfill) {
        if !submission_ids.is_empty() {
            self.send(Op::Retract { submission_ids });
            self.backfills.push_back(mode);
        }
    }

    fn ids(queue: &[QueuedInput]) -> Vec<SubmissionId> {
        queue
            .iter()
            .map(|input| input.submission_id.clone())
            .collect()
    }

    /// The `input.md` Esc rules: pending steering is applied by interrupting; without it the
    /// follow-ups come back to the draft; a second Esc while waiting restores the steering too.
    fn escape(&mut self, armed: Option<Armed>, now: Instant) {
        if self.running() {
            if self.interrupting {
                let ids = [Self::ids(&self.steering), Self::ids(&self.follow_up)].concat();
                self.retract(ids, Backfill::Merge);
            } else if !self.steering.is_empty() {
                self.interrupt();
            } else {
                self.retract(Self::ids(&self.follow_up), Backfill::Merge);
                self.interrupt();
            }
        } else if self.composer.text().trim_start().starts_with('!') {
            self.composer.clear();
            self.browsing = None;
        } else if self
            .commands
            .iter()
            .any(|command| command.info.name == "rewind")
            && self.focused
            && self.steering.is_empty()
            && self.follow_up.is_empty()
        {
            if self.composer.escape(now) {
                if self.composer.is_empty() {
                    self.send(Op::RunCommand {
                        name: "rewind".into(),
                        arguments: String::new(),
                    });
                } else {
                    self.composer.clear();
                    self.browsing = None;
                }
            }
        } else if !self.composer.is_empty() {
            if armed == Some(Armed::Clear) {
                self.composer.clear();
                self.browsing = None;
            } else {
                self.armed = Some(Armed::Clear);
                self.notice = Some("Press Esc again to clear the draft".into());
            }
        }
    }

    fn retract_newest(&mut self) {
        if let Some(input) = self.follow_up.last() {
            let id = input.submission_id.clone();
            self.retract(vec![id], Backfill::Replace);
        }
    }

    fn stash(&mut self) {
        let current = self.composer.clear();
        let previous = self.stash.take();
        if !current.is_empty() {
            self.stash = Some(current);
        }
        if let Some(previous) = previous {
            self.composer.restore(previous);
        }
        self.browsing = None;
    }

    fn content_width(&self) -> usize {
        self.width.saturating_sub(wrap::width(MARKER)).max(1)
    }

    fn browsable(&self) -> bool {
        self.composer.is_empty()
            || self.browsing.is_some_and(|(_, revision)| {
                revision == self.composer.revision() && self.composer.at_boundary()
            })
    }

    fn recall(&mut self, index: Option<usize>) {
        match index.and_then(|i| self.inputs.entries().get(i).cloned().map(|text| (i, text))) {
            Some((index, text)) => {
                self.composer.restore_parts(&text, vec![], &self.resources);
                self.browsing = Some((index, self.composer.revision()));
            }
            None => {
                self.composer.clear();
                self.browsing = None;
            }
        }
    }

    fn up(&mut self) {
        let previous = match self.browsing {
            Some((index, _)) => index.checked_sub(1),
            None => self.inputs.entries().len().checked_sub(1),
        };
        if self.browsable() && previous.is_some() {
            self.recall(previous);
        } else if !self.composer.vertical(self.content_width(), true) {
            self.composer.buffer_start();
        }
    }

    fn down(&mut self) {
        if let Some((index, _)) = self.browsing.filter(|_| self.browsable()) {
            self.recall(Some(index + 1));
        } else if !self.composer.vertical(self.content_width(), false) {
            self.composer.buffer_end();
        }
    }

    fn refresh_mention(&mut self) {
        let Some((range, query, skills)) = self.composer.resource_token() else {
            self.mention = None;
            self.dismissed = None;
            return;
        };
        let token = self.composer.text()[range.clone()].to_owned();
        if self.dismissed.as_ref() == Some(&(range.start, token)) {
            self.mention = None;
            return;
        }
        self.dismissed = None;
        let previous = self
            .mention
            .take()
            .filter(|m| m.range.start == range.start && m.skills == skills);
        if previous.is_none() {
            self.search_mode = mentions::Mode::default();
        }
        let picker_kind = self.resource_picker.as_ref().map(|(_, kind)| *kind);
        let mut matches = mentions::catalog(
            &self.resources,
            &query,
            if picker_kind.is_some() {
                mentions::Mode::All
            } else {
                self.search_mode
            },
            skills,
        );
        if let Some(kind) = picker_kind {
            matches.retain(|m| m.resource.as_ref().is_some_and(|r| r.kind == kind));
        }
        if skills && matches.is_empty() && query.starts_with(|c: char| c.is_ascii_digit()) {
            return;
        }
        if !skills && picker_kind.is_none() {
            let (root, prefix, query) = mentions::scope(&self.cwd, &query, self.home.as_deref());
            if self.index_root.as_ref().is_some_and(|old| old != &root) {
                self.release_index();
            }
            self.index_root = Some(root);
            if self.index.is_none() && self.index_state == IndexState::Idle {
                self.index_state = IndexState::Requested;
            }
            if let Some(index) = &mut self.index {
                index.query(&query, &prefix);
                matches.extend(index.rows());
            }
        }
        let previous = previous.filter(|m| m.query == query);
        let navigated = previous.as_ref().is_some_and(|m| m.navigated);
        let mut selected = 0;
        if let Some(old) = previous
            .as_ref()
            .filter(|_| navigated)
            .and_then(|m| m.matches.get(m.selected))
        {
            selected = matches
                .iter()
                .position(|m| {
                    m.path == old.path
                        && m.resource.as_ref().map(|r| &r.id)
                            == old.resource.as_ref().map(|r| &r.id)
                })
                .unwrap_or_else(|| {
                    // A streamed ranking can push the chosen file beyond the visible results.
                    if old.resource.is_none() && self.index.is_some() {
                        let at = previous
                            .as_ref()
                            .map_or(0, |m| m.selected)
                            .min(matches.len());
                        matches.insert(at, old.clone());
                        at
                    } else {
                        0
                    }
                });
        }
        self.mention = Some(Mention {
            query,
            range,
            matches,
            selected,
            skills,
            navigated,
        });
    }

    pub fn searching(&self) -> bool {
        self.mention.is_some() && self.index.as_ref().is_some_and(Index::pending)
    }

    pub fn search_step(&mut self) {
        if let Some(index) = &mut self.index {
            index.tick();
        }
        self.refresh_mention();
    }

    /// Popup keys take precedence over queueing; `None` lets the key through.
    fn mention_key(&mut self, key: &KeyEvent) -> Option<()> {
        if self.resource_picker.is_some()
            && (key.code == KeyCode::Esc
                || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.close_resource_picker();
            return Some(());
        }
        if self.resource_picker.is_some()
            && self.mention.is_none()
            && matches!(key.code, KeyCode::Enter | KeyCode::Tab)
        {
            return Some(());
        }
        let mention = self.mention.as_mut()?;
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let count = mention.matches.len().max(1);
        match key.code {
            KeyCode::BackTab if !mention.skills && self.resource_picker.is_none() => {
                self.search_mode = self.search_mode.cycle();
                self.refresh_mention();
            }
            KeyCode::Up | KeyCode::Char('p') if key.code == KeyCode::Up || control => {
                mention.selected = (mention.selected + count - 1) % count;
                mention.navigated = true;
            }
            KeyCode::Down | KeyCode::Char('n') if key.code == KeyCode::Down || control => {
                mention.selected = (mention.selected + 1) % count;
                mention.navigated = true;
            }
            KeyCode::Tab | KeyCode::Enter if key.modifiers.is_empty() => {
                match mention.matches.get(mention.selected).cloned() {
                    Some(chosen) => {
                        let mut range = mention.range.clone();
                        if self.resource_picker.is_some() {
                            self.close_resource_picker();
                            let at = self.composer.cursor();
                            if at > 0 && !self.composer.text()[..at].ends_with(char::is_whitespace)
                            {
                                self.composer.insert(" ");
                            }
                            let at = self.composer.cursor();
                            range = at..at;
                        }
                        if let Some(resource) = chosen.resource {
                            let sigil = if resource.kind == tiechui_protocol::ResourceKind::Skill {
                                '$'
                            } else {
                                '@'
                            };
                            self.composer.replace_reference(
                                range,
                                format!("{sigil}{}", resource.name),
                                resource.reference,
                            );
                        } else {
                            let text = mentions::insertion(&chosen.path);
                            if chosen.path.ends_with('/') && key.code == KeyCode::Tab {
                                self.composer.complete_path(range, &text);
                                if text.ends_with('"') {
                                    self.composer.left();
                                }
                                self.refresh_mention();
                                return Some(());
                            }
                            self.composer.replace_reference(range, text.clone(), text);
                        }
                        self.mention = None;
                    }
                    None if key.code == KeyCode::Tab => {}
                    None if self.resource_picker.is_some() => {}
                    None => return None,
                }
            }
            KeyCode::Esc => self.dismiss(),
            _ => return None,
        }
        Some(())
    }

    fn close_resource_picker(&mut self) {
        if let Some((draft, _)) = self.resource_picker.take() {
            self.composer.restore(draft);
        }
        self.mention = None;
        self.dismissed = None;
    }

    fn dismiss(&mut self) {
        self.dismissed = self
            .composer
            .resource_token()
            .map(|(range, _, _)| (range.start, self.composer.text()[range].to_owned()));
        self.mention = None;
    }

    pub fn key(&mut self, key: KeyEvent) -> Exit {
        self.key_at(key, Instant::now())
    }

    pub fn context_open(&self) -> bool {
        self.context_panel.is_some()
            || self.side_visible
            || self.agents_visible
            || self.shells_visible
            || self.pager.is_some()
            || self.resume.is_some()
    }

    pub fn take_copy(&mut self) -> Option<String> {
        self.selection.take_copy()
    }

    pub fn copy_finished(&mut self, chars: usize, sent: bool, now: Instant) {
        let unit = if chars == 1 { "char" } else { "chars" };
        let hint = if sent {
            Span::styled(
                format!("✓ Sent {chars} {unit} to clipboard"),
                self.theme.success.add_modifier(Modifier::BOLD),
            )
        } else {
            Span::styled(
                format!("Selection too long to copy ({chars} {unit})"),
                self.theme.warning.add_modifier(Modifier::BOLD),
            )
        };
        self.copy_hint = Some((hint, now + Duration::from_secs(2)));
    }

    pub fn copy_hint_pending(&self) -> bool {
        self.copy_hint.is_some()
    }

    pub fn expire_copy_hint(&mut self, now: Instant) {
        if self
            .copy_hint
            .as_ref()
            .is_some_and(|(_, until)| now >= *until)
        {
            self.copy_hint = None;
        }
    }

    fn paint_copy_hint(&mut self, lines: &mut [Line<'static>]) {
        self.expire_copy_hint(Instant::now());
        if let Some((hint, _)) = &self.copy_hint
            && let Some(footer) = lines.last_mut()
        {
            *footer = wrap::fit(vec![Span::raw("  "), hint.clone()], self.width, false);
        }
    }

    pub fn mouse(&mut self, event: crossterm::event::MouseEvent) {
        use crossterm::event::{MouseButton, MouseEventKind};
        if !self.context_open() && !self.fullscreen {
            return;
        }
        self.pointer = Some((usize::from(event.row), usize::from(event.column)));
        if matches!(
            event.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) {
            self.selection.clear();
        }
        if !self.context_open() && usize::from(event.row) >= self.main_body_height {
            if matches!(event.kind, MouseEventKind::Down(_)) {
                self.selection.clear();
            }
            if !self.selection.active()
                || !matches!(event.kind, MouseEventKind::Drag(_) | MouseEventKind::Up(_))
            {
                return;
            }
        }
        if let Some(panel) = &mut self.context_panel
            && !panel.text_view()
        {
            self.selection.clear();
            panel.mouse(event);
            return;
        }
        let event = match self
            .selection
            .mouse(event, Instant::now(), self.copy_on_select)
        {
            crate::selection::MouseAction::Handled => return,
            crate::selection::MouseAction::Click(event) => event,
            crate::selection::MouseAction::Pass => event,
        };
        self.composer.reset_escape();
        if self.side_visible
            && let Some(side) = &mut self.side
        {
            side.mouse(event);
            return;
        }
        if let Some(panel) = &mut self.context_panel {
            panel.mouse(event);
            return;
        }
        if event.kind == MouseEventKind::Down(MouseButton::Left)
            && self.pager.is_none()
            && let Some(control) =
                self.control_at(usize::from(event.row), usize::from(event.column))
        {
            self.transcript.unfold(control.cell);
            self.main_view.reveal(control.start);
            return;
        }
        let delta = match event.kind {
            crossterm::event::MouseEventKind::ScrollUp => -3,
            crossterm::event::MouseEventKind::ScrollDown => 3,
            _ => return,
        };
        if let Some(pager) = &mut self.pager {
            pager.scroll(delta);
        } else if !self.context_open() {
            self.main_view.scroll(delta);
        } else {
            for _ in 0..3 {
                self.key_inner(
                    KeyEvent::new(
                        if delta < 0 {
                            KeyCode::Up
                        } else {
                            KeyCode::Down
                        },
                        KeyModifiers::NONE,
                    ),
                    Instant::now(),
                );
            }
        }
    }

    /// The main view's control row under the pointer, including label padding.
    fn control_at(&mut self, row: usize, column: usize) -> Option<crate::transcript::Control> {
        let line = self.main_view.line(row)?;
        let on =
            crate::cells::on_control(self.transcript.retained().get(line)?, column, self.width);
        self.transcript.control(line).filter(|_| on)
    }

    pub fn focus(&mut self, focused: bool) {
        self.paste_generation += 1;
        if !focused {
            self.selection.clear();
        }
        self.focused = focused;
        self.composer.reset_escape();
    }
    pub fn escape_pending(&self) -> bool {
        self.composer.escape_pending()
    }
    pub fn expire_escape(&mut self, now: Instant) {
        self.composer.expire_escape(now);
    }
    fn refill(&mut self, content: Vec<InputContent>) {
        self.composer.reset_escape();
        let (text, images) = parts(&content);
        self.refilled = !images.is_empty();
        self.saved_refill = self.refilled;
        self.composer.clear();
        self.composer.restore_parts(&text, images, &self.resources);
        self.browsing = None;
        self.refresh_menu();
    }
    fn clear_saved_refill(&mut self) {
        if std::mem::take(&mut self.saved_refill) {
            self.send(Op::ClearDraft);
        }
    }
    pub fn key_at(&mut self, key: KeyEvent, now: Instant) -> Exit {
        self.copy_hint = None;
        self.paste_generation += 1;
        if key.kind == KeyEventKind::Release
            || (key.code == KeyCode::Esc && key.kind == KeyEventKind::Repeat)
        {
            return Exit::Running;
        }
        let modal = self.agents_visible
            || self.shells_visible
            || self.side_visible
            || self.context_panel.is_some()
            || self.pager.is_some()
            || self.resume.is_some()
            || self.prompt.is_some()
            || self.select.is_some()
            || self.picker.is_some()
            || self.theme_picker.is_some()
            || self.help
            || self.menu.is_some()
            || self.mention.is_some();
        let ready = !modal
            && (!self.fullscreen || self.main_view.following())
            && !self.running()
            && self.focused
            && !self.composer.text().trim_start().starts_with('!');
        if key.code != KeyCode::Esc || !ready {
            self.composer.reset_escape();
        }
        if !modal
            && self.goal_status.is_some()
            && (key.code == KeyCode::Esc
                || key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL))
        {
            self.send(Op::InterruptIdle);
        }
        let images = self.composer.image_count();
        let plain_escape = KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE);
        let exit = if key.code == KeyCode::Esc && key.modifiers == KeyModifiers::ALT {
            let exit = self.key_inner(plain_escape, now);
            if ready {
                self.key_inner(plain_escape, now)
            } else {
                exit
            }
        } else {
            self.key_inner(key, now)
        };
        if self.composer.image_count() < images {
            self.clear_saved_refill();
        }
        exit
    }
    fn key_inner(&mut self, key: KeyEvent, now: Instant) -> Exit {
        if self.selection.key(key) {
            return Exit::Running;
        }
        if let Some(picker) = &mut self.theme_picker {
            use crate::theme_picker::Outcome;
            match picker.key(key) {
                Outcome::Preview(syntax) => self.theme.syntax = syntax,
                Outcome::Cancel(syntax) => {
                    self.theme.syntax = syntax;
                    self.theme_picker = None;
                }
                Outcome::Save(name) => {
                    self.theme_name = name.clone();
                    self.save_theme = Some(name);
                    self.theme_picker = None;
                }
            }
            self.theme_changed();
            return Exit::Running;
        }
        if self.agents_visible
            && let Some(panel) = &mut self.agents
        {
            match panel.key(key) {
                crate::subagents::Outcome::Open => {}
                crate::subagents::Outcome::Close => self.agents_visible = false,
                crate::subagents::Outcome::Cancel(id) => {
                    self.send(Op::CommandInput {
                        name: "subagents".into(),
                        data: serde_json::json!({"cancel":id}),
                    });
                }
            }
            return Exit::Running;
        }
        if self.shells_visible
            && let Some(panel) = &mut self.shells
        {
            match panel.key(key) {
                crate::shells::Outcome::Open => {}
                crate::shells::Outcome::Close => self.shells_visible = false,
                crate::shells::Outcome::Send(data) => {
                    self.send(Op::CommandInput {
                        name: "shells".into(),
                        data,
                    });
                }
            }
            return Exit::Running;
        }
        if key.kind == KeyEventKind::Release {
            return Exit::Running;
        }
        if self.side_visible
            && let Some(side) = &mut self.side
        {
            match side.key(key, &self.status_line) {
                crate::btw::Outcome::Open => {}
                crate::btw::Outcome::Switch => self.side_visible = false,
                crate::btw::Outcome::Save(config) => {
                    self.status_line = config.clone();
                    self.save = Some(config);
                }
                crate::btw::Outcome::Action(action) => {
                    if let Ok(data) = serde_json::to_value(action) {
                        self.send(Op::CommandInput {
                            name: "btw".into(),
                            data,
                        });
                    }
                }
            }
            return Exit::Running;
        }
        if let Some(panel) = &mut self.context_panel {
            if !panel.key(key) {
                self.context_panel = None;
            }
            return Exit::Running;
        }
        if let Some(pager) = &mut self.pager {
            if !pager.key(key) {
                self.pager = None;
            }
            return Exit::Running;
        }
        if let Some(resume) = &mut self.resume {
            let fresh = self.thread.is_none();
            match resume.key(key) {
                resume::Outcome::Open => {}
                resume::Outcome::Quit => return Exit::Quit,
                resume::Outcome::Pick(path) => {
                    self.resume = None;
                    self.switch = Some(Target::Resume(path));
                }
                resume::Outcome::Cancel => {
                    self.resume = None;
                    // Leaving the startup picker starts a new session.
                    if fresh {
                        self.switch = Some(Target::New);
                    }
                }
            }
            return Exit::Running;
        }
        if let Some((approval_id, prompt)) = &mut self.prompt {
            let (decision, interrupt) = match prompt.key(key) {
                prompt::Outcome::Open => return Exit::Running,
                prompt::Outcome::Answer(decision) => (decision, false),
                prompt::Outcome::Interrupt(decision) => (decision, true),
            };
            let approval_id = approval_id.clone();
            self.close_prompt(now);
            self.send(Op::ApprovalResponse {
                approval_id,
                decision,
            });
            if interrupt && let Some(turn_id) = self.turn.clone() {
                self.send(Op::Interrupt { turn_id });
            }
            return Exit::Running;
        }
        if let Some(select) = &mut self.select {
            match select.key(key) {
                select::Outcome::Open => {}
                select::Outcome::Cancel => self.select = None,
                select::Outcome::Pick(op) => {
                    self.select = None;
                    self.send(op);
                }
            }
            return Exit::Running;
        }
        if let Some(picker) = &mut self.picker {
            match picker.key(key) {
                Outcome::Open => {}
                Outcome::Cancel => self.picker = None,
                Outcome::Save(config) => {
                    self.picker = None;
                    self.status_line = config.clone();
                    self.save = Some(config);
                }
            }
            return Exit::Running;
        }
        if self.fullscreen && self.menu.is_none() && self.mention.is_none() {
            match key.code {
                KeyCode::PageUp | KeyCode::PageDown => {
                    self.main_view.key(key);
                    return Exit::Running;
                }
                KeyCode::Home | KeyCode::End if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.main_view.key(key);
                    return Exit::Running;
                }
                KeyCode::Esc if !self.main_view.following() => {
                    self.main_view.follow();
                    return Exit::Running;
                }
                _ => {}
            }
        }
        if self.composer.is_empty()
            && key.modifiers.contains(KeyModifiers::ALT)
            && matches!(key.code, KeyCode::Left | KeyCode::Right)
            && self.commands.iter().any(|c| c.info.name == "subagents")
        {
            self.send(Op::RunCommand {
                name: "subagents".into(),
                arguments: String::new(),
            });
            return Exit::Running;
        }
        if self.side.is_some() && crate::btw::switch_key(key) {
            self.side_visible = true;
            return Exit::Running;
        }
        let control = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        let plain = !control && !alt;
        let bursting = self
            .burst
            .is_some_and(|(last, count)| count >= 2 && now.duration_since(last) < BURST_GAP);
        if plain && matches!(key.code, KeyCode::Char(_)) {
            self.burst = Some(match self.burst {
                Some((last, count)) if now.duration_since(last) < BURST_GAP => (now, count + 1),
                _ => (now, 1),
            });
        } else if bursting && plain && matches!(key.code, KeyCode::Enter | KeyCode::Tab) {
            // Inside an unbracketed paste these are text, never submission or queueing.
            self.burst = self.burst.map(|(_, count)| (now, count));
            if key.code == KeyCode::Enter {
                self.composer.newline();
            } else {
                self.composer.insert("    ");
            }
            self.refresh_mention();
            return Exit::Running;
        } else {
            self.burst = None;
        }
        let armed = self.armed.take();
        self.notice = None;
        if let Some(forward) = self.jump.take()
            && let KeyCode::Char(target) = key.code
        {
            self.composer.jump(target, forward);
            self.refresh_mention();
            return Exit::Running;
        }
        if std::mem::take(&mut self.help) && matches!(key.code, KeyCode::Esc | KeyCode::Char('?')) {
            return Exit::Running;
        }
        if let Some(exit) = self.menu_key(&key) {
            self.refresh_menu();
            return exit;
        }
        if self.mention_key(&key).is_some() {
            return Exit::Running;
        }
        let running = self.running();
        let c = &mut self.composer;
        match key.code {
            KeyCode::Char('c') if control => {
                if !c.is_empty() {
                    c.clear();
                    self.browsing = None;
                } else if running {
                    self.interrupt();
                } else if armed == Some(Armed::Quit) {
                    return Exit::Quit;
                } else {
                    self.armed = Some(Armed::Quit);
                    self.notice = Some("Press Ctrl+C again to quit".into());
                }
            }
            KeyCode::Char('d') if control && c.is_empty() => return Exit::Quit,
            KeyCode::Char('d') if control => c.delete_forward(),
            KeyCode::Char('z') if control => return Exit::Suspend,
            KeyCode::Char('a') if control => c.home(),
            KeyCode::Char('e') if control => c.end(),
            KeyCode::Char('b') if control => c.left(),
            KeyCode::Char('f') if control => c.right(),
            KeyCode::Char('w') if control => c.delete_word_backward(),
            KeyCode::Char('u') if control => c.delete_to_line_start(),
            KeyCode::Char('k') if control => c.delete_to_line_end(),
            KeyCode::Char('y') if control => c.yank(),
            KeyCode::Char('j') if control => c.newline(),
            // Ctrl+- arrives as Ctrl+_ or Ctrl+7 without the kitty protocol.
            KeyCode::Char('-' | '_' | '7') if control => c.undo(),
            KeyCode::Char(']') if control => self.jump = Some(!alt),
            KeyCode::Char('o') if control => self.pager = Some(Pager::default()),
            KeyCode::Char('t') if control => {
                let shown = self.transcript.toggle_thinking();
                self.notice = Some(
                    if shown {
                        "Thinking shown"
                    } else {
                        "Thinking hidden"
                    }
                    .into(),
                );
            }
            KeyCode::Char('b') if alt => c.word_backward(),
            KeyCode::Char('f') if alt => c.word_forward(),
            KeyCode::Char('d') if alt => c.delete_word_forward(),
            KeyCode::Char('y') if alt => c.yank_pop(),
            KeyCode::Char('s') if alt => self.stash(),
            KeyCode::Esc => self.escape(armed, now),
            KeyCode::Enter if shift => c.newline(),
            KeyCode::Enter if alt => self.submit(InputDelivery::FollowUp),
            KeyCode::Enter if running => self.submit(InputDelivery::Steer),
            // Shell drafts run on Enter only.
            KeyCode::Tab if c.text().trim_start().starts_with('!') => {}
            KeyCode::Enter | KeyCode::Tab => self.submit(InputDelivery::FollowUp),
            KeyCode::Backspace if alt || control => c.delete_word_backward(),
            KeyCode::Backspace => c.backspace(),
            KeyCode::Delete if alt => c.delete_word_forward(),
            KeyCode::Delete => c.delete_forward(),
            KeyCode::Left if shift && plain => self.retract_newest(),
            KeyCode::Left if alt || control => c.word_backward(),
            KeyCode::Left => c.left(),
            KeyCode::Right if alt || control => c.word_forward(),
            KeyCode::Right => c.right(),
            KeyCode::Up if alt => self.retract_newest(),
            KeyCode::Up => self.up(),
            KeyCode::Down => self.down(),
            KeyCode::Home => c.home(),
            KeyCode::End => c.end(),
            KeyCode::PageUp | KeyCode::PageDown => {
                let width = self.content_width();
                for _ in 0..self.composer_rows() {
                    self.composer.vertical(width, key.code == KeyCode::PageUp);
                }
            }
            KeyCode::Char('?') if plain && c.is_empty() => self.help = true,
            KeyCode::Char(ch) if plain => c.insert(ch.encode_utf8(&mut [0; 4])),
            _ => {}
        }
        self.refresh_mention();
        self.refresh_menu();
        if std::mem::take(&mut self.quit) {
            return Exit::Quit;
        }
        Exit::Running
    }

    pub fn clipboard_ticket(&self) -> Option<PasteTicket> {
        if !self.focused
            || self.prompt.is_some()
            || self.resume.is_some()
            || self.pager.is_some()
            || self.context_panel.is_some()
            || self.agents_visible
            || self.shells_visible
            || self.theme_picker.is_some()
        {
            return None;
        }
        let composer = if self.side_visible {
            &self.side.as_ref()?.composer
        } else {
            &self.composer
        };
        let draft = composer.draft();
        Some(PasteTicket {
            draft,
            revision: composer.revision(),
            side_id: self
                .side
                .as_ref()
                .filter(|_| self.side_visible)
                .map(|s| s.view.id.clone()),
            side: self.side_visible,
            thread: self.thread.clone(),
            generation: self.paste_generation,
        })
    }

    pub fn clipboard_received(&mut self, ticket: PasteTicket, content: Option<paste::Clipboard>) {
        let Some(current) = self.clipboard_ticket() else {
            return;
        };
        if ticket.revision != current.revision
            || ticket.side_id != current.side_id
            || ticket.draft != current.draft
            || ticket.side != current.side
            || ticket.thread != current.thread
            || ticket.generation != current.generation
        {
            return;
        }
        match content {
            Some(paste::Clipboard::Image(mime, data)) => {
                let composer = if self.side_visible {
                    let Some(side) = &mut self.side else {
                        return;
                    };
                    &mut side.composer
                } else {
                    &mut self.composer
                };
                if !composer.text().trim_start().starts_with('!') {
                    composer.insert_image(mime, data);
                }
            }
            Some(paste::Clipboard::Text(text)) => self.paste(&text),
            None => self.notice = Some("Clipboard has no supported image or text".into()),
        }
        self.refresh_mention();
        self.refresh_menu();
    }

    /// Bracketed paste: normalized, image paths become attachments, long text collapses.
    pub fn paste(&mut self, text: &str) {
        self.selection.clear();
        if self.theme_picker.is_some() {
            for ch in text.chars().filter(|ch| !ch.is_control()) {
                self.key_inner(
                    KeyEvent::new(KeyCode::Char(ch), KeyModifiers::NONE),
                    Instant::now(),
                );
            }
            return;
        }
        self.composer.reset_escape();
        self.paste_inner(text);
    }
    fn paste_inner(&mut self, text: &str) {
        if self.agents_visible || self.shells_visible {
            return;
        }
        if self.side_visible
            && let Some(side) = &mut self.side
        {
            side.paste_with_path(text, &self.cwd);
            return;
        }
        if let Some(panel) = &mut self.context_panel {
            panel.paste(text);
            return;
        }
        if let Some(resume) = &mut self.resume {
            resume.paste(&paste::normalize(text));
            return;
        }
        if let Some((_, prompt)) = &mut self.prompt {
            prompt.paste(&paste::normalize(text));
            return;
        }
        self.help = false;
        self.notice = None;
        let text = paste::normalize(text);
        if !self.composer.text().trim_start().starts_with('!')
            && let Some((mime_type, data)) = paste::image(&text, &self.cwd)
        {
            self.composer.insert_image(mime_type, data);
        } else if composer::should_collapse(&text) {
            self.composer.insert_paste(&text);
        } else if !text.is_empty() {
            self.composer.insert(&text);
        }
        self.refresh_mention();
        self.refresh_menu();
    }

    fn composer_rows(&self) -> usize {
        let rows = (self.height / 2).max(1);
        if self.menu.is_some() || self.mention.is_some() {
            rows.min(
                (self.height * 2 / 3)
                    .max(8)
                    .min(self.height)
                    .saturating_sub(5)
                    .max(1),
            )
        } else {
            rows
        }
    }

    fn rule(&self, hidden: usize, arrow: &str) -> Line<'static> {
        let label = format!(" {arrow} {hidden} more ");
        let room = self.width.saturating_sub(2 + wrap::width(&label));
        let rule = self.theme.rule(self.focused);
        if hidden == 0 || room == 0 {
            return Line::from(Span::styled("─".repeat(self.width), rule));
        }
        Line::from(vec![
            Span::styled("──", rule),
            Span::styled(label, self.theme.dim),
            Span::styled("─".repeat(room), rule),
        ])
    }

    fn preview(
        &self,
        lines: &mut Vec<Line<'static>>,
        title: &str,
        queue: &[QueuedInput],
        italic: bool,
    ) {
        if queue.is_empty() {
            return;
        }
        lines.push(Line::default());
        lines.extend(wrap::wrap(
            &[Span::raw(title.to_owned())],
            self.width,
            &[Span::raw("  "), Span::styled("• ", self.theme.dim)],
            &[Span::raw("    ")],
        ));
        let mut style = self.theme.dim;
        if italic {
            style = style.add_modifier(Modifier::ITALIC);
        }
        let inner = self.width.saturating_sub(6).max(1);
        for input in queue {
            let text = self.drafts.get(&input.submission_id).map_or_else(
                || parts(&input.content).0,
                |(draft, _)| draft.text().to_owned(),
            );
            let wrapped: Vec<Line<'static>> = text
                .split('\n')
                .flat_map(|line| wrap::wrap(&[Span::raw(line.to_owned())], inner, &[], &[]))
                .collect();
            for (index, line) in wrapped.iter().take(3).enumerate() {
                let mut spans = vec![Span::styled(
                    if index == 0 { "    ↳ " } else { "      " },
                    style,
                )];
                spans.extend(
                    line.spans
                        .iter()
                        .map(|s| Span::styled(s.content.clone(), style)),
                );
                lines.push(Line::from(spans));
            }
            if wrapped.len() > 3 {
                lines.push(Line::from(Span::styled("      …", style)));
            }
        }
    }

    fn help_lines(&self) -> Vec<Line<'static>> {
        let mut lines = vec![Line::default()];
        for (key, text) in SHORTCUTS {
            let key = if key.is_empty() { self.queue_hint } else { key };
            let line = vec![
                Span::raw(format!("  {key:<13}")),
                Span::styled(text, self.theme.dim),
            ];
            lines.extend(wrap::wrap(
                &line,
                self.width,
                &[],
                &[Span::raw(" ".repeat(15))],
            ));
        }
        lines
    }

    /// The full-screen transcript while the pager is open.
    pub fn animating(&self) -> bool {
        self.running()
            || (self.side_visible && self.side.as_ref().is_some_and(|side| side.view.busy))
    }
    pub fn page_cursor(&self) -> Option<(usize, usize)> {
        self.side
            .as_ref()
            .filter(|_| self.side_visible)
            .and_then(|side| side.cursor)
    }
    pub fn pager_frame(&mut self, rows: usize) -> Option<Vec<Line<'static>>> {
        let page = self.page_inner(rows);
        if page.is_none() && !self.fullscreen {
            self.selection.clear();
        }
        page.map(|page| {
            let mut page = self.selection.frame(page);
            self.paint_copy_hint(&mut page);
            page
        })
    }

    fn page_inner(&mut self, rows: usize) -> Option<Vec<Line<'static>>> {
        if self.shells_visible
            && let Some(panel) = &mut self.shells
        {
            return Some(panel.render(self.width, rows, &self.theme));
        }
        if self.agents_visible
            && let Some(mut panel) = self.agents.take()
        {
            let width = self.width.saturating_sub(29);
            self.cache_expanded(width);
            let committed = self.expanded.2.len();
            self.expanded.2.extend(self.transcript.expanded_live(width));
            let page = panel.render(
                self.width,
                rows,
                &self.theme,
                &self.expanded.2,
                self.prompt.is_some(),
            );
            self.expanded.2.truncate(committed);
            self.agents = Some(panel);
            return Some(page);
        }
        if self.side_visible
            && let Some(mut side) = self.side.take()
        {
            let main_status = if matches!(&self.prompt, Some((_, Prompt::Permission(_)))) {
                "needs approval"
            } else if self.prompt.is_some() {
                "needs input"
            } else if self.running() {
                ""
            } else {
                match self.session.last {
                    Some(TurnStatus::Completed) => "done",
                    Some(TurnStatus::Interrupted) => "interrupted",
                    Some(TurnStatus::Failed) => "failed",
                    _ => "",
                }
            };
            side.focused = self.focused;
            let lines = side.render(
                self.width,
                rows,
                &self.theme,
                &self.snapshot(),
                &self.status_line,
                main_status,
            );
            self.side = Some(side);
            return Some(lines);
        }
        if let Some(panel) = &mut self.context_panel {
            panel.needs_input(self.prompt.is_some());
            return Some(panel.render(self.width, rows, &self.theme));
        }
        if let Some(resume) = &mut self.resume {
            return Some(resume.render(self.width, rows, &self.theme));
        }
        self.pager.as_ref()?;
        let width = self.width;
        self.cache_expanded(width);
        let committed = self.expanded.2.len();
        self.expanded.2.extend(self.transcript.expanded_live(width));
        let theme = self.theme.clone();
        let page = self
            .pager
            .as_mut()
            .map(|pager| pager.render(&self.expanded.2, width, rows, &theme));
        self.expanded.2.truncate(committed);
        page
    }

    fn cache_expanded(&mut self, width: usize) {
        let count = self.transcript.cell_count();
        if self.expanded.1 != width || self.expanded.0 > count {
            self.expanded = (0, width, Vec::new());
        }
        self.expanded
            .2
            .extend(self.transcript.expanded(self.expanded.0, width));
        self.expanded.0 = count;
    }

    fn phase(&self) -> Phase {
        if self.running() {
            if self.prompt.is_some() || self.retry_waiting {
                Phase::Waiting
            } else if self.compaction.is_some() {
                Phase::Compacting
            } else if self.transcript.tools_running() {
                Phase::Working
            } else {
                Phase::Thinking
            }
        } else {
            match self.session.last {
                Some(TurnStatus::Failed) => Phase::Error,
                Some(TurnStatus::Interrupted) => Phase::Cancelled,
                _ => Phase::Ready,
            }
        }
    }

    fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            cwd: &self.cwd,
            home: self.home.as_deref(),
            hostname: self.hostname.as_deref(),
            model: self.session.model.as_ref(),
            thinking: self.session.thinking.unwrap_or(ThinkingLevel::Off),
            context: self.session.context.as_ref(),
            total: &self.session.total,
            compacted: self.session.compacted,
            phase: self.phase(),
            permission: self.permission,
            thread: self.thread.as_ref().map(|(id, _)| id.0.as_str()),
            git: &self.git,
        }
    }

    /// `Working (5s • esc to interrupt)`, the current thinking heading, or a compaction under
    /// `◇` timed from its start; the label gives way first on narrow terminals so the hint stays.
    fn working(&self, active: Duration, phase: Duration, now: Instant) -> Line<'static> {
        let compaction = self.compaction.as_ref().filter(|_| !self.retry_waiting);
        let label = if self.retry_waiting {
            "Retrying request".to_owned()
        } else if let Some((reason, _, step)) = compaction {
            let label = match reason {
                tiechui_protocol::CompactionReason::Manual => "Compacting context",
                tiechui_protocol::CompactionReason::Threshold => "Auto-compacting",
                tiechui_protocol::CompactionReason::Overflow => "Context overflow, compacting",
            };
            if step.is_empty() {
                label.to_owned()
            } else {
                format!("{label} · {step}")
            }
        } else if self.transcript.tools_running() {
            "Working".to_owned()
        } else {
            self.transcript.status().unwrap_or("Working").to_owned()
        };
        let (bullet, suffix) = match compaction {
            Some((_, started, _)) => {
                let active = now.saturating_duration_since(*started);
                (
                    activity::diamond(&self.theme, phase, active),
                    format!(" ({} • esc to cancel)", elapsed(active)),
                )
            }
            None => (
                activity::bullet(&self.theme, phase, active),
                format!(" ({} • esc to interrupt)", elapsed(active)),
            ),
        };
        let room = self.width.saturating_sub(2 + wrap::width(&suffix));
        let label = wrap::fit(vec![Span::raw(label)], room.max(1), true);
        let label: String = label.spans.iter().map(|s| s.content.as_ref()).collect();
        let mut spans = vec![bullet, Span::raw(" ")];
        spans.extend(activity::shimmer(&label, &self.theme, phase));
        spans.push(Span::styled(suffix, self.theme.dim));
        wrap::fit(spans, self.width, false)
    }

    /// Streaming tail, live tools, working status, queued previews, then the composer between
    /// two rules, with notices and the mention popup or shortcuts below.
    pub fn live(&mut self) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        let now = Instant::now();
        let lines = self.transcript.live(now - self.epoch, now);
        self.bottom(lines, now)
    }

    pub fn fullscreen(&self) -> bool {
        self.fullscreen
    }

    /// The transcript scrolls independently of the bottom input and interaction area.
    pub fn main_frame(&mut self) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        self.transcript.discard_history();
        let now = Instant::now();
        let (bottom, cursor) = self.bottom(Vec::new(), now);
        let limit = if self.prompt.is_some()
            || self.select.is_some()
            || self.picker.is_some()
            || self.theme_picker.is_some()
        {
            self.height
        } else {
            (self.height * 2 / 3).max(8).min(self.height)
        };
        let skip = bottom.len().saturating_sub(limit);
        let bottom = &bottom[skip..];
        let available = self.height.saturating_sub(bottom.len());
        self.main_body_height = available;
        let tail = self.transcript.retained_live(now - self.epoch, now);
        let hint = !self.main_view.following() && available > 0;
        let body_height = available.saturating_sub(usize::from(hint));
        let mut body = if body_height == 0 {
            Vec::new()
        } else {
            self.main_view
                .viewport_parts(self.transcript.retained(), &tail, body_height)
        };
        body.resize(body_height, Line::default());
        if let Some((row, column)) = self.pointer
            && self.control_at(row, column).is_some()
            && let Some(line) = body.get_mut(row)
        {
            *line = crate::cells::hovered(line, &self.theme);
        }
        if hint {
            body.push(wrap::fit(
                vec![Span::styled(
                    "  esc latest · pgup/pgdn scroll",
                    self.theme.dim,
                )],
                self.width,
                false,
            ));
        }
        let mut frame = self.selection.frame(body);
        frame.truncate(available);
        frame.resize(available, Line::default());
        frame.extend_from_slice(bottom);
        self.paint_copy_hint(&mut frame);
        let cursor = cursor
            .filter(|(row, _)| *row >= skip && !self.selection.active())
            .map(|(row, column)| (available + row - skip, column));
        (frame, cursor)
    }

    /// Background shells, the goal and scheduled tasks on one row under the status line,
    /// absent while none runs; the shell count always leads and survives narrow screens.
    fn activity(&self) -> Option<Line<'static>> {
        let mut items: Vec<Vec<Span<'static>>> = Vec::new();
        if let Some(count) = &self.shell_indicator {
            items.push(vec![
                Span::styled(count.clone(), self.theme.accent2),
                Span::styled(" · /shells", self.theme.dim),
            ]);
        }
        if let Some(text) = &self.goal_status {
            items.push(vec![Span::styled(format!("◈ {text}"), self.theme.accent2)]);
        }
        if let Some(text) = &self.loop_status {
            items.push(vec![Span::styled(format!("◇ {text}"), self.theme.dim)]);
        }
        let width = self.width.saturating_sub(2);
        let join = |items: &[Vec<Span<'static>>]| {
            let mut spans = vec![Span::raw("  ")];
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    spans.push(Span::raw("   "));
                }
                spans.extend(item.iter().cloned());
            }
            spans
        };
        let mut spans = join(&items);
        if self.shell_indicator.is_some() && wrap::line_width(&Line::from(spans.clone())) > width {
            items[0].pop();
            spans = join(&items);
        }
        (!items.is_empty()).then(|| wrap::fit(spans, width, true))
    }

    fn bottom(
        &mut self,
        mut lines: Vec<Line<'static>>,
        now: Instant,
    ) -> (Vec<Line<'static>>, Option<(usize, usize)>) {
        if let Some(side) = &self.side {
            lines.push(Line::from(Span::styled(
                if side.view.busy {
                    "  Side working · ctrl+/ for side"
                } else {
                    "  ctrl+/ for side"
                },
                self.theme.dim,
            )));
        }
        // A prompt replaces the working status and the composer.
        if let Some(started) = self
            .started
            .filter(|_| !self.transcript.answering() && self.prompt.is_none())
        {
            lines.push(Line::default());
            lines.push(
                self.working(
                    self.paused
                        .unwrap_or(now)
                        .saturating_duration_since(started),
                    now - self.epoch,
                    now,
                ),
            );
        }
        self.preview(
            &mut lines,
            "Messages to be submitted after next tool call",
            &self.steering,
            false,
        );
        self.preview(&mut lines, "Queued follow-up inputs", &self.follow_up, true);
        if !self.follow_up.is_empty() {
            let indent = [Span::raw("      ")];
            lines.extend(wrap::wrap(
                &[Span::styled(
                    format!("{} edit last queued message", self.queue_hint),
                    self.theme.dim,
                )],
                self.width,
                &indent,
                &indent,
            ));
        }
        lines.push(Line::default());
        if let Some(picker) = &self.theme_picker {
            lines.extend(picker.render(
                self.width,
                self.height.saturating_sub(lines.len() + 1),
                &self.theme,
            ));
            lines.push(status::line(
                &self.status_line,
                &self.snapshot(),
                self.width,
                &self.theme,
            ));
            return (lines, None);
        }
        if let Some((_, prompt)) = &mut self.prompt {
            let top = lines.len();
            let (rows, cursor) = prompt.render(self.width, self.height, &self.theme);
            lines.extend(rows);
            return (lines, cursor.map(|(row, column)| (top + row, column)));
        }
        if let Some(select) = &self.select {
            let top = lines.len();
            let (rows, (row, column)) = select.render(self.width, self.height, &self.theme);
            lines.extend(rows);
            return (lines, Some((top + row, column)));
        }
        if let Some(mut picker) = self.picker.take() {
            let top = lines.len();
            // The screen keeps the bottom rows: the picker may push the gap above it out.
            let (rows, (row, column)) =
                picker.render(&self.snapshot(), self.width, self.height, &self.theme);
            lines.extend(rows);
            self.picker = Some(picker);
            return (lines, Some((top + row, column)));
        }
        let shell = self.composer.text().trim_start().starts_with('!');
        let marker = if shell {
            Span::styled("! ", self.theme.error.add_modifier(Modifier::BOLD))
        } else {
            Span::styled(MARKER, Style::default().add_modifier(Modifier::BOLD))
        };
        let element = Style::default().fg(Color::Cyan);
        let rows = self.composer_rows();
        let mut rendered = self.composer.render(self.width, rows, marker, element);
        // A leading `!` becomes the marker itself.
        if shell
            && rendered.above == 0
            && let Some(text) = rendered.lines.first_mut().and_then(|l| l.spans.get_mut(1))
            && let Some(rest) = text.content.strip_prefix('!')
        {
            text.content = rest.to_owned().into();
            let indent = wrap::width(MARKER);
            if rendered.cursor.0 == 0 {
                rendered.cursor.1 = rendered.cursor.1.saturating_sub(1);
                if rendered.cursor.1 < indent {
                    rendered.cursor.1 = 0;
                }
            }
        }
        let budget = (self.height * 2 / 3).max(8).min(self.height);
        let footer = 3
            + self.plugin_status.len()
            + usize::from(
                self.goal_status.is_some()
                    || self.loop_status.is_some()
                    || self.shell_indicator.is_some(),
            )
            + usize::from(self.stash.is_some())
            + usize::from(self.notice.is_some())
            + usize::from(self.retry_countdown.is_some())
            + usize::from(self.composer.escape_pending());
        let menu_height = budget.saturating_sub(rendered.lines.len() + footer);
        if let Some(menu) = &self.menu {
            lines.extend(commands::popup(
                &self.commands,
                &menu.matches,
                menu.selected,
                self.composer
                    .text()
                    .trim_start()
                    .strip_prefix('/')
                    .unwrap_or_default(),
                self.width,
                menu_height,
                &self.theme,
            ));
        } else if let Some(mention) = &self.mention {
            let files = !mention.skills && self.resource_picker.is_none();
            let loading = files
                && ((self.index_state != IndexState::Idle && self.index.is_none())
                    || self.index.as_ref().is_some_and(Index::pending));
            let warning = self
                .index
                .as_ref()
                .filter(|_| files)
                .and_then(|index| index.warning.as_deref());
            let status = if let Some(warning) = warning {
                warning.into()
            } else if loading {
                format!(
                    "Scanning… {} paths",
                    self.index.as_ref().map_or(0, Index::discovered)
                )
            } else if mention.matches.is_empty() {
                if mention.skills {
                    "No matching skills"
                } else {
                    "No matches"
                }
                .into()
            } else if mention.skills {
                mention
                    .matches
                    .get(mention.selected)
                    .and_then(|m| m.resource.as_ref())
                    .map_or(String::new(), |r| {
                        format!("{} · {}", r.description, r.path.as_deref().unwrap_or(""))
                    })
            } else {
                let resources = mention
                    .matches
                    .iter()
                    .filter(|m| m.resource.is_some())
                    .count();
                format!(
                    "{} matches",
                    resources
                        + self
                            .index
                            .as_ref()
                            .filter(|_| files)
                            .map_or(0, Index::match_count)
                )
            };
            lines.extend(mentions::popup(
                &mention.matches,
                mention.selected,
                mention.skills,
                self.search_mode,
                &status,
                self.width,
                menu_height,
                &self.theme,
            ));
        }
        lines.push(self.rule(rendered.above, "↑"));
        let top = lines.len();
        lines.extend(rendered.lines);
        lines.push(self.rule(rendered.below, "↓"));
        if self.composer.escape_pending() {
            let hint = if self.composer.is_empty() {
                "esc again to rewind"
            } else {
                "esc again to clear input"
            };
            lines.push(Line::from(Span::styled(
                format!("  {hint}"),
                self.theme.dim,
            )));
        }
        if let Some(notice) = &self.notice {
            lines.push(Line::from(Span::styled(
                format!("  {notice}"),
                self.theme.dim,
            )));
        }
        if let Some((attempt, maximum, deadline)) = self.retry_countdown {
            let seconds = deadline
                .saturating_duration_since(now)
                .as_millis()
                .div_ceil(1000);
            let maximum = maximum.map_or_else(|| "?".into(), |n| n.to_string());
            lines.extend(wrap::wrap(
                &[Span::styled(
                    format!("Retry {attempt}/{maximum} in {seconds}s · Esc cancel · /retry now"),
                    self.theme.dim,
                )],
                self.width,
                &[Span::raw("  ")],
                &[Span::raw("  ")],
            ));
        }
        if self.menu.is_none() && self.mention.is_none() && self.help {
            lines.extend(self.help_lines());
        }
        for (plugin, text) in &self.plugin_status {
            lines.push(wrap::fit(
                vec![Span::styled(format!("  {plugin}: {text}"), self.theme.dim)],
                self.width,
                true,
            ));
        }
        lines.push(status::line(
            &self.status_line,
            &self.snapshot(),
            self.width,
            &self.theme,
        ));
        lines.extend(self.activity());
        if self.stash.is_some() {
            lines.push(wrap::fit(
                vec![Span::styled(
                    "  Draft stashed · alt+s restore / swap · restores after submit",
                    self.theme.dim,
                )],
                self.width,
                true,
            ));
        }
        let (row, column) = rendered.cursor;
        (lines, Some((top + row, column)))
    }
}
