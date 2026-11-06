//! Interactive terminal client for a Tiechui session, driven only by protocol events.

pub mod activity;
mod app;
mod ask;
pub mod btw;
pub mod cells;
pub mod commands;
mod completion;
pub mod composer;
pub mod consent;
pub mod context;
pub mod copy;
pub mod diff;
mod editor;
mod elicitation;
mod goal;
pub mod header;
mod history;
pub mod information;
pub mod markdown;
pub mod mentions;
pub mod pager;
pub mod palette;
mod paste;
pub use paste::Clipboard;
mod permission;
mod plugin;
mod program;
mod prompt;
pub mod render;
pub mod resume;
pub mod rewind;
mod select;
pub mod selection;
pub mod shells;
pub mod status;
mod status_picker;
mod stream;
pub mod subagents;
pub mod syntax;
pub mod terminal;
pub mod theme;
pub mod theme_picker;
pub mod tools;
mod transcript;
pub mod wrap;

use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event as TermEvent, EventStream,
    KeyboardEnhancementFlags, PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{disable_raw_mode, enable_raw_mode, size, supports_keyboard_enhancement};
use futures::StreamExt;
use futures::future::LocalBoxFuture;
use tiechui_protocol::{Event, Op, SessionEpoch, Submission};
use tokio::sync::{broadcast, mpsc};

pub use activity::elapsed;
pub use app::{App, Exit, queue_hint};

use crate::palette::{Palette, detect_color_level};
use crate::render::Screen;
use crate::theme::Theme;

pub struct Client {
    pub submissions: mpsc::Sender<Submission>,
    pub events: broadcast::Receiver<Event>,
}

/// The session `/new`, `/resume` or `/reload` asks the host for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    New,
    Resume(PathBuf),
    /// The current session again, with its configuration and resources read anew.
    Reload,
}

pub struct Opened {
    pub client: Client,
    pub cwd: PathBuf,
    pub status_line: status::StatusLineConfig,
    pub copy_on_select: bool,
    pub syntax_theme: Option<String>,
    /// What preparing the session reported.
    pub warnings: Vec<String>,
}

/// Ends the current session, then opens the one asked for at the given epoch: above every
/// epoch the client has seen, so nothing from the old session passes for the new one's.
pub type Switch =
    Box<dyn FnMut(Target, SessionEpoch) -> LocalBoxFuture<'static, Result<Opened, String>>>;

pub struct Host {
    pub switch: Switch,
    pub services: Arc<dyn Services>,
}

/// Blocking storage operations owned by the embedding application.
pub trait Services: Send + Sync {
    fn read(
        &self,
        cwd: &Path,
        read: resume::Read,
        cancel: &tokio_util::sync::CancellationToken,
    ) -> resume::Loaded;
    fn save_status_line(&self, line: &status::StatusLineConfig) -> Result<(), String>;
    fn save_theme(&self, name: &str) -> Result<(), String>;
    fn save_model(&self, model: &str) -> Result<(), String>;
    fn trust(&self, cwd: &Path, arguments: &str) -> String;
}

#[derive(Debug)]
pub struct Options {
    /// Preserve the terminal's native scrollback instead of owning a fullscreen transcript.
    pub no_alt_screen: bool,
    pub syntax_theme: Option<String>,
    pub themes_dir: Option<PathBuf>,
    /// Sent once the first thread starts, as `chui exec` does: the command line's model and
    /// thinking level, persisted by the session.
    pub startup: Vec<Op>,
    /// Root for `@` mentions and relative pasted image paths.
    pub cwd: PathBuf,
    /// `input-history.jsonl`; `None` keeps history in memory only.
    pub history: Option<PathBuf>,
    /// The retrieval key advertised in the queue preview; `run` detects it from the terminal.
    pub queue_hint: Option<&'static str>,
    /// Shown as `~` in the status line.
    pub home: Option<PathBuf>,
    /// The hostname item's value; `run` reads the machine's when unset.
    pub hostname: Option<String>,
    pub status_line: status::StatusLineConfig,
    pub copy_on_select: bool,
    /// Starts with the resume picker instead of a session; leaving it starts a new one.
    pub pick: bool,
    /// Prints the session header when a session opens, as the interactive UI does.
    pub banner: bool,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            no_alt_screen: false,
            syntax_theme: None,
            themes_dir: None,
            startup: Vec::new(),
            cwd: PathBuf::new(),
            history: None,
            queue_hint: None,
            home: None,
            hostname: None,
            status_line: status::StatusLineConfig::default(),
            copy_on_select: true,
            pick: false,
            banner: false,
        }
    }
}

static ENHANCED: AtomicBool = AtomicBool::new(false);
static ALT_ENHANCED: AtomicBool = AtomicBool::new(false);
static ALTERNATE: AtomicBool = AtomicBool::new(false);

fn alternate(out: &mut impl Write, enter: bool) -> io::Result<()> {
    if ALTERNATE.swap(enter, Ordering::SeqCst) != enter {
        if !enter && ALT_ENHANCED.swap(false, Ordering::SeqCst) {
            execute!(out, PopKeyboardEnhancementFlags)?;
        }
        write!(out, "{}", if enter { "\x1b[?1049h" } else { "\x1b[?1049l" })?;
        // The alternate buffer has an independent keyboard-mode stack.
        if enter && ENHANCED.load(Ordering::SeqCst) {
            enhance(out)?;
            ALT_ENHANCED.store(true, Ordering::SeqCst);
        }
    }
    Ok(())
}

fn restore() {
    let mut out = io::stdout();
    let _ = alternate(&mut out, false);
    if ENHANCED.swap(false, Ordering::SeqCst) {
        let _ = execute!(out, PopKeyboardEnhancementFlags);
    }
    let _ = execute!(
        out,
        DisableBracketedPaste,
        crossterm::event::DisableFocusChange
    );
    let _ = execute!(out, crossterm::event::DisableMouseCapture);
    let _ = write!(out, "\x1b[?2026l\x1b[0m\x1b[?25h");
    let _ = out.flush();
    let _ = disable_raw_mode();
}

fn setup(out: &mut impl Write, fullscreen: bool) -> io::Result<()> {
    enable_raw_mode()?;
    if fullscreen {
        alternate(out, true)?;
    }
    execute!(
        out,
        EnableBracketedPaste,
        crossterm::event::EnableFocusChange,
        crossterm::event::DisableMouseCapture
    )?;
    // Disambiguated keys make Shift+Enter, Alt combinations and a lone Esc reliable.
    if supports_keyboard_enhancement().unwrap_or(false) {
        enhance(out)?;
        if fullscreen {
            ALT_ENHANCED.store(true, Ordering::SeqCst);
        } else {
            ENHANCED.store(true, Ordering::SeqCst);
        }
    }
    Ok(())
}

fn enhance(out: &mut impl Write) -> io::Result<()> {
    execute!(
        out,
        PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
    )
}

/// Runs until the user quits or the session ends. The terminal is restored on every exit path.
/// `client` is `None` when the resume picker comes first.
pub async fn run(client: Option<Client>, host: Host, options: Options) -> io::Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err(io::Error::other(
            "the interactive UI needs a terminal; use chui exec",
        ));
    }
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore();
        previous(info);
    }));
    let mut out = io::stdout();
    let result = match setup(&mut out, !options.no_alt_screen) {
        Ok(()) => session(client, host, options, &mut out).await,
        Err(error) => Err(error),
    };
    restore();
    result
}

async fn next_event(client: &mut Option<Client>) -> Result<Event, broadcast::error::RecvError> {
    match client {
        Some(client) => client.events.recv().await,
        None => std::future::pending().await,
    }
}

async fn session(
    mut client: Option<Client>,
    mut host: Host,
    mut options: Options,
    out: &mut io::Stdout,
) -> io::Result<()> {
    write!(out, "\x1b]10;?\x1b\\\x1b]11;?\x1b\\\x1b[c")?;
    out.flush()?;
    let env = |key: &str| std::env::var(key).ok();
    let theme = Theme::new(Palette {
        level: detect_color_level(&env),
        foreground: None,
        background: None,
    });
    let (columns, rows) = size()?;
    let mut height = usize::from(rows);
    options.queue_hint = options.queue_hint.or(Some(queue_hint(&env)));
    options.hostname = options.hostname.or_else(status::hostname);
    let mut app = App::new(theme, usize::from(columns), options);
    app.resize(usize::from(columns), height);
    let mut screen = Screen::default();
    let mut canvas = render::Canvas::default();
    let reflow_cap = terminal::reflow_cap(&env);
    let tmux = std::env::var_os("TMUX").is_some_and(|value| !value.is_empty());
    let mut keys = EventStream::new();
    let mut replies = terminal::ReplyFilter::default();
    let mut switching: Option<LocalBoxFuture<'static, Result<Opened, String>>> = None;
    // Fast enough for the working shimmer's two-second sweep.
    let mut tick = tokio::time::interval(Duration::from_millis(100));
    let mut search_tick = tokio::time::interval(Duration::from_millis(16));
    search_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let clipboard_cancel = tokio_util::sync::CancellationToken::new();
    let _clipboard_guard = clipboard_cancel.clone().drop_guard();
    let (pasted, mut pastes) = mpsc::channel(1);
    let (indexed, mut index) = mpsc::channel(1);
    let (fetched, mut git) = mpsc::channel(1);
    let (failed, mut failures) = mpsc::channel(1);
    let (reported, mut reports) = mpsc::channel::<(&'static str, String)>(1);
    let (read, mut reads) = mpsc::channel(8);
    let mut mouse = false;
    'session: loop {
        if mouse != (app.fullscreen() || app.context_open()) {
            mouse = app.fullscreen() || app.context_open();
            if mouse {
                execute!(out, crossterm::event::EnableMouseCapture)?;
            } else {
                execute!(out, crossterm::event::DisableMouseCapture)?;
            }
        }
        if let Some(text) = app.take_copy() {
            let sequence = copy::encode(&text, tmux);
            if let Some(sequence) = &sequence {
                out.write_all(sequence.as_bytes())?;
                out.flush()?;
            }
            app.copy_finished(text.chars().count(), sequence.is_some(), Instant::now());
        }
        // Requests first, so the startup picker's listing starts before any key.
        if let Some((cwd, generation, cancel)) = app.take_index_request() {
            let indexed = indexed.clone();
            tokio::spawn(async move {
                let built = mentions::build(cwd, cancel.clone()).await;
                if !cancel.is_cancelled() {
                    let _ = indexed.send((generation, built)).await;
                }
            });
        }
        if let Some((cwd, fields)) = app.take_git_request() {
            let fetched = fetched.clone();
            tokio::spawn(async move {
                let _ = fetched.send((fields, status::git(cwd, fields).await)).await;
            });
        }
        if let Some(line) = app.take_save() {
            let services = host.services.clone();
            let failed = failed.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(error) = services.save_status_line(&line) {
                    let _ = failed.blocking_send(format!("Status line not saved: {error}"));
                }
            });
        }
        if let Some(name) = app.take_theme_save() {
            let services = host.services.clone();
            let failed = failed.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(error) = services.save_theme(&name) {
                    let _ = failed.blocking_send(format!("Theme not saved: {error}"));
                }
            });
        }
        if let Some(model) = app.take_model_save() {
            let services = host.services.clone();
            let failed = failed.clone();
            tokio::task::spawn_blocking(move || {
                if let Err(error) = services.save_model(&model) {
                    let _ = failed.blocking_send(format!("Model not saved: {error}"));
                }
            });
        }
        // One switch at a time: a later request waits in the app.
        let shown = app.thread().is_some();
        if switching.is_none()
            && let Some((target, epoch)) = app.take_switch()
        {
            // The startup picker leaves the shell's output alone.
            if shown && target != Target::Reload && !app.fullscreen() {
                alternate(out, false)?;
                screen.wipe(out)?;
            }
            client = None;
            switching = Some((host.switch)(target, epoch));
        }
        if let Some(batch) = app.take_reads() {
            for request in batch.reads {
                let (read, cancel) = (read.clone(), batch.cancel.clone());
                let services = host.services.clone();
                let cwd = app.cwd().to_owned();
                let generation = batch.generation;
                tokio::task::spawn_blocking(move || {
                    let loaded = services.read(&cwd, request, &cancel);
                    if !cancel.is_cancelled() {
                        let _ = read.blocking_send((generation, loaded));
                    }
                });
            }
        }
        if let Some(arguments) = app.take_trust() {
            let reported = reported.clone();
            let services = host.services.clone();
            let cwd = app.cwd().to_owned();
            tokio::task::spawn_blocking(move || {
                let text = services.trust(&cwd, &arguments);
                let _ = reported.blocking_send(("trust", text));
            });
        }
        if let Some(client) = &client {
            for submission in app.take_submissions() {
                if client.submissions.send(submission).await.is_err() {
                    if !app.fullscreen() {
                        screen.clear(out)?;
                    }
                    return Ok(());
                }
            }
        }
        if let Some(page) = app.pager_frame(height) {
            alternate(out, true)?;
            canvas.draw(out, &page, app.page_cursor())?;
        } else if app.fullscreen() {
            let (frame, cursor) = app.main_frame();
            canvas.draw(out, &frame, cursor)?;
        } else {
            alternate(out, false)?;
            canvas.invalidate();
            let reflow = app.reflow(Instant::now(), reflow_cap);
            if reflow.is_some() {
                write!(out, "\x1b[?2026h")?;
                screen.wipe(out)?;
            }
            if reflow.is_some() || !app.reflow_pending() {
                let (live, cursor) = app.live();
                let history = reflow.unwrap_or_else(|| app.take_history());
                screen.frame(out, &history, &live, cursor, height)?;
            }
        }
        tokio::select! {
            _ = search_tick.tick(), if app.searching() => app.search_step(),
            key = keys.next() => match key {
                Some(Ok(TermEvent::Key(key))) => {
                    let (keys, colors) = replies.key(key);
                    if let Some((foreground, background)) = colors { app.colors(foreground, background); }
                    for key in keys {
                    if key.code == crossterm::event::KeyCode::Char('v') && key.modifiers == crossterm::event::KeyModifiers::CONTROL && key.kind != crossterm::event::KeyEventKind::Release {
                        if let Some(ticket) = app.clipboard_ticket() {
                            let pasted = pasted.clone();
                            let cancel = clipboard_cancel.child_token();
                            tokio::spawn(async move { let content = paste::clipboard(cancel).await; let _ = pasted.send((ticket, content)).await; });
                        }
                        continue;
                    }
                    match app.key(key) {
                    Exit::Running => {}
                    Exit::Quit => break 'session,
                    Exit::Suspend => {
                        if !app.fullscreen() { screen.clear(out)?; }
                        restore();
                        suspend();
                        setup(out, app.fullscreen())?;
                        canvas.invalidate();
                        mouse = false;
                    }
                    } }
                },
                Some(Ok(TermEvent::FocusLost)) => app.focus(false),
                Some(Ok(TermEvent::FocusGained)) => app.focus(true),
                Some(Ok(TermEvent::Paste(text))) => app.paste(&text),
                Some(Ok(TermEvent::Mouse(event))) => app.mouse(event),
                Some(Ok(TermEvent::Resize(columns, rows))) => {
                    height = usize::from(rows);
                    app.resize(usize::from(columns), height);
                    canvas.invalidate();
                }
                Some(Err(error)) => return Err(error),
                None => break,
            },
            event = next_event(&mut client) => match event {
                Ok(event) => app.event(event),
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
            Some(opened) = async { Some(switching.as_mut()?.await) }, if switching.is_some() => {
                switching = None;
                let opened = opened.map_err(io::Error::other)?;
                client = Some(opened.client);
                app.reload_theme(opened.syntax_theme);
                app.opened(opened.cwd, opened.status_line, opened.copy_on_select, opened.warnings);
            }
            Some((ticket, content)) = pastes.recv() => app.clipboard_received(ticket, content),
            Some((generation, loaded)) = reads.recv() => app.loaded(generation, loaded),
            Some((generation, built)) = index.recv() => app.set_index_result(generation, built),
            Some((fields, facts)) = git.recv() => app.set_git(fields, facts),
            Some(message) = failures.recv() => app.set_notice(message),
            Some((name, text)) = reports.recv() => app.command_output(name, text),
            _ = tick.tick(), if app.animating() || app.escape_pending() || app.reflow_pending() || app.copy_hint_pending() => {
                let now = Instant::now();
                app.expire_escape(now);
                app.expire_copy_hint(now);
            }
        }
    }
    if app.fullscreen() {
        return Ok(());
    }
    alternate(out, false)?;
    let history = app.take_history();
    screen.frame(out, &history, &[], None, height)?;
    screen.clear(out)
}

#[cfg(unix)]
fn suspend() {
    // SAFETY: raising SIGTSTP stops the process until SIGCONT, as a shell's Ctrl-Z would.
    unsafe {
        libc::raise(libc::SIGTSTP);
    }
}

#[cfg(not(unix))]
fn suspend() {}
