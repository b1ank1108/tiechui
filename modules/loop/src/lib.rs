//! Scheduled prompts: `/loop`, the cron tools and the idle scheduler.
mod cron;
mod store;

use std::collections::hash_map::RandomState;
use std::fs::File;
use std::hash::BuildHasher;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use cron::{Cron, System, Zone};
use store::{Store, Task};
use tiechui_plugin_api::{
    model::*,
    protocol::CommandInfo,
    serde_json::{Value, json},
    *,
};

const KIND: &str = "tiechui.loop.run";
const STATUS: &str = "tiechui.loop.status";
const MAX_TASKS: usize = 50;
const MAX_AGE: i64 = 7 * 1440 * cron::MINUTE;

/// Task state for the process, so session-only tasks survive `/reload` of their session.
#[derive(Clone, Default)]
pub struct Schedules(Arc<Mutex<State>>);

pub struct LoopModule {
    /// Runs due tasks; single-run sessions only manage durable ones.
    pub schedule: bool,
    /// Project root for durable tasks; `None` when the project is untrusted.
    pub root: Option<PathBuf>,
    pub schedules: Schedules,
}

impl Module for LoopModule {
    fn id(&self) -> &str {
        "loop"
    }

    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        let inner = Arc::new(Loop {
            schedules: self.schedules.clone(),
            store: self.root.clone().map(Store::new),
            schedule: self.schedule,
            driver: CancellationToken::new(),
            started: AtomicBool::new(false),
        });
        if self.schedule {
            registry.register_runtime(inner.clone());
            for name in ["loop", "cron-list", "cron-delete"] {
                registry.register_command(Arc::new(LoopCommand {
                    inner: inner.clone(),
                    name,
                }));
            }
        }
        for (name, description, schema) in [
            (
                "cron_create",
                CREATE,
                json!({"type":"object","additionalProperties":false,"required":["cron","prompt"],"properties":{
                    "cron":{"type":"string","description":"Five fields in local time: minute hour day-of-month month day-of-week."},
                    "prompt":{"type":"string","minLength":1,"description":"Run as if the user sent it when the task fires."},
                    "recurring":{"type":"boolean","description":"Default true. false runs once, then deletes the task."},
                    "durable":{"type":"boolean","description":"Default false. true saves the task in the project across sessions."}
                }}),
            ),
            (
                "cron_list",
                "List scheduled tasks: this session's and the project's durable ones.",
                json!({"type":"object","additionalProperties":false,"properties":{}}),
            ),
            (
                "cron_delete",
                "Cancel a scheduled task by ID, session-only or durable. A run in progress is not stopped.",
                json!({"type":"object","additionalProperties":false,"required":["id"],"properties":{"id":{"type":"string"}}}),
            ),
        ] {
            registry.register_tool(Arc::new(CronTool {
                inner: inner.clone(),
                spec: ToolSpec {
                    name: name.into(),
                    description: description.into(),
                    input_schema: schema,
                },
            }))?;
        }
        Ok(())
    }
}

const CREATE: &str = "Schedule a prompt to run later in this session, once or on a recurring cron schedule. Use it only when the user asks for something to happen later or repeatedly.

The cron has 5 fields in the user's local time: minute hour day-of-month month day-of-week. \"0 9 * * *\" is 9am local.

One-shot (recurring: false) pins minute, hour, day of month and month, e.g. \"30 14 3 11 *\" for 2:30pm on Nov 3, runs once and is deleted. Recurring tasks (the default), e.g. \"*/5 * * * *\", \"7 * * * *\" or \"0 9 * * 1-5\", expire after 7 days: they run one last time and are deleted. Tell the user about this limit.

When the requested time is approximate, pick a minute other than :00 or :30. Tasks run only while the session is idle, with a small stable offset: recurring up to 10% of the period late (at most 15 minutes), one-shot on :00 or :30 up to 90 seconds early. Missed periods are not replayed.

Tasks are session-only by default. Pass durable: true only when the user explicitly wants the task kept across sessions; it is saved to .agents/scheduled_tasks.json and runs while Tiechui is open in this project. Creating a task does not run it now. Returns the ID for cron_delete.";

#[derive(Default)]
pub struct State {
    session: String,
    tasks: Vec<Entry>,
    owner: Option<File>,
    owned_since: i64,
    /// Durable one-shots that came due while no session ran them; the user decides.
    missed: Vec<Task>,
    /// Durable changes from runs, not yet written.
    writes: Vec<Write>,
    running: Option<String>,
    generation: u64,
    driver: u64,
    error: Option<String>,
    reported: Option<String>,
}

struct Entry {
    task: Task,
    durable: bool,
    due: Option<i64>,
}

impl Entry {
    fn new(task: Task, durable: bool, zone: &dyn Zone) -> Self {
        let due = schedule(&task, zone);
        Self { task, durable, due }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Write {
    Fired(String, i64),
    Remove(String),
}

fn schedule(task: &Task, zone: &dyn Zone) -> Option<i64> {
    let cron = Cron::parse(&task.cron).ok()?;
    if task.recurring {
        let anchor = task.last_fired_at.unwrap_or(task.created_at);
        cron::recurring_fire(&cron, anchor, &task.id, zone)
    } else {
        cron::one_shot_fire(&cron, task.created_at, &task.id, zone)
    }
}

fn apply(writes: &[Write], tasks: &mut Vec<Task>) {
    for write in writes {
        match write {
            Write::Fired(id, at) => {
                if let Some(task) = tasks.iter_mut().find(|t| &t.id == id) {
                    task.last_fired_at = Some(*at);
                }
            }
            Write::Remove(id) => tasks.retain(|t| &t.id != id),
        }
    }
}

impl State {
    fn runnable(&self) -> impl Iterator<Item = &Entry> {
        self.tasks
            .iter()
            .filter(|e| !e.durable || self.owner.is_some())
    }

    /// The missed-task question first, then the earliest due task.
    fn due(&self, now: i64, zone: &dyn Zone) -> Option<Continuation> {
        let (identity, text, visible) = if self.missed.is_empty() {
            let entry = self
                .runnable()
                .filter(|e| e.due.is_some_and(|due| due <= now))
                .min_by_key(|e| (e.due, e.task.created_at, e.task.id.clone()))?;
            (entry.task.id.clone(), entry.task.prompt.clone(), true)
        } else {
            (String::new(), missed(&self.missed, zone), false)
        };
        Some(Continuation {
            kind: KIND.into(),
            generation: self.generation,
            identity,
            text,
            visible,
        })
    }

    /// Advances a task as its run starts; a crash before the write lands can repeat it once.
    fn fired(&mut self, id: &str, now: i64, zone: &dyn Zone) {
        self.running = Some(id.into());
        self.generation += 1;
        if id.is_empty() {
            for task in std::mem::take(&mut self.missed) {
                self.write(Write::Remove(task.id));
            }
            return;
        }
        let Some(index) = self.tasks.iter().position(|e| e.task.id == id) else {
            return;
        };
        let entry = &mut self.tasks[index];
        if entry.task.recurring && now - entry.task.created_at < MAX_AGE {
            entry.task.last_fired_at = Some(now);
            entry.due = schedule(&entry.task, zone);
            if entry.durable {
                self.write(Write::Fired(id.into(), now));
            }
        } else if self.tasks.remove(index).durable {
            self.write(Write::Remove(id.into()));
        }
    }

    fn write(&mut self, write: Write) {
        let (Write::Fired(id, _) | Write::Remove(id)) = &write;
        self.writes
            .retain(|w| !matches!(w, Write::Fired(other, _) | Write::Remove(other) if other == id));
        self.writes.push(write);
    }

    /// Replaces the durable view with the file plus changes not yet written.
    fn load(&mut self, mut tasks: Vec<Task>, zone: &dyn Zone) {
        apply(&self.writes, &mut tasks);
        self.tasks.retain(|e| !e.durable);
        for task in tasks {
            let entry = Entry::new(task, true, zone);
            if self.missed.iter().any(|t| t.id == entry.task.id) {
                continue;
            }
            if self.owner.is_some()
                && !entry.task.recurring
                && entry.due.is_some_and(|due| due < self.owned_since)
            {
                self.missed.push(entry.task);
            } else {
                self.tasks.push(entry);
            }
        }
        self.generation += 1;
    }

    fn status(&self, now: i64, zone: &dyn Zone) -> Option<String> {
        let count = self.tasks.len() + self.missed.len();
        if count == 0 {
            return None;
        }
        let mut text = format!(
            "{count} scheduled task{}",
            if count == 1 { "" } else { "s" }
        );
        match self.runnable().filter_map(|e| e.due).min() {
            Some(at) => {
                let (next, today) = (zone.local(at), zone.local(now));
                text +=
                    &if (next.year, next.month, next.day) == (today.year, today.month, today.day) {
                        format!(" · next {:02}:{:02}", next.hour, next.minute)
                    } else {
                        format!(
                            " · next {:02}-{:02} {:02}:{:02}",
                            next.month, next.day, next.hour, next.minute
                        )
                    };
            }
            None if self.owner.is_none() && self.tasks.iter().any(|e| e.durable) => {
                text += " · durable tasks run in another session";
            }
            None => {}
        }
        Some(text)
    }
}

fn missed(tasks: &[Task], zone: &dyn Zone) -> String {
    let (subject, have, these, each) = if tasks.len() == 1 {
        ("task was", "It has", "this prompt", "it")
    } else {
        ("tasks were", "They have", "these prompts", "each one")
    };
    let mut text = format!(
        "The following one-shot scheduled {subject} missed while no Tiechui session was running for this project. {have} been removed from {}.\n\nDo not run {these} yet. First ask the user whether to run {each} now, and run only what they confirm.",
        store::FILE
    );
    for task in tasks {
        let longest = task
            .prompt
            .split(|c| c != '`')
            .map(str::len)
            .max()
            .unwrap_or(0);
        let fence = "`".repeat((longest + 1).max(3));
        text += &format!(
            "\n\n[{}, created {}]\n{fence}\n{}\n{fence}",
            cron::describe(&task.cron),
            cron::format(task.created_at, zone),
            task.prompt
        );
    }
    text
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn fresh(taken: impl Fn(&str) -> bool) -> String {
    loop {
        let id = format!("{:08x}", RandomState::new().hash_one(now()) as u32);
        if !taken(&id) {
            return id;
        }
    }
}

fn check(prompt: &str) -> Result<(), String> {
    let first = prompt.split_whitespace().next().unwrap_or_default();
    match first.strip_prefix('/') {
        _ if first.is_empty() => Err("A scheduled prompt cannot be empty.".into()),
        Some(name)
            if !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "-_:".contains(c)) =>
        {
            Err(format!(
                "/{name} is a command, and commands cannot be scheduled. Reference a skill as ${name} instead."
            ))
        }
        _ => Ok(()),
    }
}

struct Loop {
    schedules: Schedules,
    store: Option<Store>,
    schedule: bool,
    driver: CancellationToken,
    started: AtomicBool,
}

impl Drop for Loop {
    fn drop(&mut self) {
        self.driver.cancel();
    }
}

struct Spec {
    cron: String,
    prompt: String,
    recurring: bool,
    durable: bool,
    /// Runs once as soon as the session is idle.
    now: bool,
}

struct Listed {
    task: Task,
    durable: bool,
    due: Option<i64>,
}

impl Listed {
    fn json(&self, zone: &dyn Zone) -> Value {
        json!({
            "id": self.task.id,
            "cron": self.task.cron,
            "human_schedule": cron::describe(&self.task.cron),
            "prompt": self.task.prompt,
            "recurring": self.task.recurring,
            "durable": self.durable,
            "next_run_at": self.due.map(|at| cron::format(at, zone)),
            "expires_at": self.task.recurring.then(|| cron::format(self.task.created_at + MAX_AGE, zone)),
        })
    }

    /// The schedule, then the prompt's first line, shortened.
    fn lines(&self, zone: &dyn Zone) -> String {
        let first = self.task.prompt.lines().next().unwrap_or_default();
        let prompt: String = first.chars().take(80).collect();
        format!(
            "{} · {}{} · {} · next {}\n  {prompt}{}",
            self.task.id,
            if self.task.recurring { "" } else { "once " },
            cron::describe(&self.task.cron),
            if self.durable { "durable" } else { "session" },
            self.due
                .map_or_else(|| "never".into(), |at| cron::format(at, zone)),
            if prompt.len() < self.task.prompt.trim_end().len() {
                "…"
            } else {
                ""
            }
        )
    }
}

impl Loop {
    fn lock(&self) -> Result<MutexGuard<'_, State>, String> {
        self.schedules.0.lock().map_err(|e| e.to_string())
    }

    async fn blocking<T: Send + 'static>(
        work: impl FnOnce() -> Result<T, String> + Send + 'static,
    ) -> Result<T, String> {
        tokio::task::spawn_blocking(work)
            .await
            .map_err(|e| e.to_string())?
    }

    async fn create(&self, spec: Spec, cancel: CancellationToken) -> Result<Listed, String> {
        let at = now();
        let parsed = Cron::parse(&spec.cron)?;
        if parsed.next(at, &System).is_none() {
            return Err(format!(
                "Cron expression '{}' does not match any date in the next year.",
                spec.cron
            ));
        }
        check(&spec.prompt)?;
        if self.lock()?.running.is_some() {
            return Err("A scheduled run cannot create scheduled tasks.".into());
        }
        let limit = format!("Too many scheduled tasks (at most {MAX_TASKS}). Delete one first.");
        let template = Task {
            id: String::new(),
            cron: spec.cron,
            prompt: spec.prompt,
            created_at: at,
            last_fired_at: None,
            recurring: spec.recurring,
        };
        if !spec.durable {
            if !self.schedule {
                return Err("Session-only tasks need an interactive session. Pass durable: true to keep the task in the project.".into());
            }
            let mut s = self.lock()?;
            if s.tasks.len() >= MAX_TASKS {
                return Err(limit);
            }
            let id = fresh(|id| s.tasks.iter().any(|e| e.task.id == id));
            let mut entry = Entry::new(Task { id, ..template }, false, &System);
            if spec.now {
                entry.due = Some(at);
            }
            let listed = Listed {
                task: entry.task.clone(),
                durable: false,
                due: schedule(&entry.task, &System),
            };
            s.tasks.push(entry);
            s.generation += 1;
            return Ok(listed);
        }
        let store = self.store.clone().ok_or(
            "Durable tasks need a trusted project. Trust it with `chui trust`, or keep the task session-only.",
        )?;
        let session: Vec<String> = self
            .lock()?
            .tasks
            .iter()
            .filter(|e| !e.durable)
            .map(|e| e.task.id.clone())
            .collect();
        let claim = self.schedule;
        let ((id, tasks), owner) = Self::blocking(move || {
            let created = store.update(&cancel, |tasks| {
                if tasks.len() + session.len() >= MAX_TASKS {
                    return Err(limit);
                }
                let id =
                    fresh(|id| tasks.iter().any(|t| t.id == id) || session.iter().any(|s| s == id));
                tasks.push(Task {
                    id: id.clone(),
                    ..template
                });
                Ok(id)
            })?;
            Ok((created, claim.then(|| store.claim()).flatten()))
        })
        .await?;
        let mut s = self.lock()?;
        if s.owner.is_none()
            && let Some(owner) = owner
        {
            s.owner = Some(owner);
            s.owned_since = at;
        }
        s.load(tasks, &System);
        s.tasks
            .iter()
            .find(|e| e.task.id == id)
            .map(|e| Listed {
                task: e.task.clone(),
                durable: true,
                due: e.due,
            })
            .ok_or_else(|| "The task was saved but could not be read back.".into())
    }

    async fn list(&self) -> Result<Vec<Listed>, String> {
        let durable = match self.store.clone() {
            Some(store) => Self::blocking(move || store.read()).await?,
            None => vec![],
        };
        let s = self.lock()?;
        let mut listed: Vec<_> = s
            .tasks
            .iter()
            .filter(|e| !e.durable)
            .map(|e| Listed {
                task: e.task.clone(),
                durable: false,
                due: e.due,
            })
            .collect();
        listed.extend(durable.into_iter().map(|task| {
            Listed {
                due: s
                    .tasks
                    .iter()
                    .find(|e| e.task.id == task.id)
                    .map_or_else(|| schedule(&task, &System), |e| e.due),
                task,
                durable: true,
            }
        }));
        Ok(listed)
    }

    async fn delete(&self, id: &str, cancel: CancellationToken) -> Result<String, String> {
        let session = {
            let mut s = self.lock()?;
            s.generation += 1;
            s.missed.retain(|t| t.id != id);
            let index = s.tasks.iter().position(|e| !e.durable && e.task.id == id);
            index.map(|index| s.tasks.remove(index)).is_some()
        };
        if !session {
            let missing = || format!("No scheduled task with ID {id}.");
            let store = self.store.clone().ok_or_else(missing)?;
            let target = id.to_owned();
            let (found, tasks) = Self::blocking(move || {
                if !store.read()?.iter().any(|t| t.id == target) {
                    return Ok((false, vec![]));
                }
                store.update(&cancel, |tasks| {
                    tasks.retain(|t| t.id != target);
                    Ok(true)
                })
            })
            .await?;
            if !found {
                return Err(missing());
            }
            self.lock()?.load(tasks, &System);
        }
        Ok(if self.lock()?.running.as_deref() == Some(id) {
            format!("Deleted {id}. Its current run continues; press Esc to stop it.")
        } else {
            format!("Deleted {id}.")
        })
    }

    fn start(&self) {
        let (Some(store), Ok(runtime)) =
            (self.store.clone(), tokio::runtime::Handle::try_current())
        else {
            return;
        };
        let Ok(mut s) = self.lock() else {
            return;
        };
        s.driver += 1;
        runtime.spawn(drive(
            self.schedules.clone(),
            store,
            s.driver,
            self.driver.clone(),
        ));
    }
}

/// Keeps the durable view current, holds the owner role and writes run updates.
async fn drive(schedules: Schedules, store: Store, driver: u64, cancel: CancellationToken) {
    let lock = || schedules.0.lock().ok();
    let (mut stamp, mut probed) = (None, 0);
    loop {
        let stopping = cancel.is_cancelled();
        let writes = lock().map(|s| s.writes.clone()).unwrap_or_default();
        if !writes.is_empty() {
            let (store, flushed) = (store.clone(), writes.clone());
            let result = Loop::blocking(move || {
                store.update(&CancellationToken::new(), |tasks| {
                    apply(&flushed, tasks);
                    Ok(())
                })
            })
            .await;
            let Some(mut s) = lock() else { return };
            match result {
                Ok(_) => {
                    s.writes.retain(|w| !writes.contains(w));
                    stamp = None;
                }
                Err(error) => s.error = Some(error),
            }
        }
        if stopping {
            return;
        }
        let at = now();
        if lock().is_some_and(|s| s.owner.is_none()) && at - probed >= 5000 {
            probed = at;
            let claimer = store.clone();
            if let Ok(Some(owner)) = Loop::blocking(move || Ok(claimer.claim())).await {
                let Some(mut s) = lock() else { return };
                if s.driver == driver && s.owner.is_none() {
                    s.owner = Some(owner);
                    s.owned_since = at;
                    stamp = None;
                }
            }
        }
        let reader = store.clone();
        let current = Loop::blocking(move || Ok(reader.stamp()))
            .await
            .ok()
            .flatten();
        if stamp != Some(current) {
            let reader = store.clone();
            let result = Loop::blocking(move || reader.read()).await;
            let Some(mut s) = lock() else { return };
            if s.driver != driver {
                return;
            }
            match result {
                Ok(tasks) => {
                    s.error = None;
                    s.load(tasks, &System);
                }
                Err(error) => {
                    s.error = Some(error);
                    s.tasks.retain(|e| !e.durable);
                }
            }
            stamp = Some(current);
        }
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_secs(1)) => {}
        }
    }
}

impl RuntimeExtension for Loop {
    fn event(&self, event: &RuntimeEvent, branch: &BranchView) -> Result<RuntimeOutput, ToolError> {
        let start = matches!(event, RuntimeEvent::Restore { .. })
            && !self.started.swap(true, Ordering::Relaxed);
        let mut s = self.lock().map_err(ToolError)?;
        let at = now();
        match event {
            RuntimeEvent::Restore { .. } => {
                if s.session != branch.session_id {
                    s.session = branch.session_id.clone();
                    s.tasks.retain(|e| e.durable);
                }
                s.running = None;
                s.generation += 1;
            }
            RuntimeEvent::BeforeNavigate => s.generation += 1,
            RuntimeEvent::Input {
                continuation: Some(c),
                ..
            } if c.kind == KIND => s.fired(&c.identity, at, &System),
            RuntimeEvent::Input { .. } | RuntimeEvent::Settled(_) => s.running = None,
            RuntimeEvent::Shutdown => {
                self.driver.cancel();
                s.driver += 1;
                s.owner = None;
                s.running = None;
                s.missed.clear();
                s.tasks.retain(|e| !e.durable);
            }
            _ => {}
        }
        let mut output = RuntimeOutput::default();
        if s.error != s.reported {
            s.reported = s.error.clone();
            if let Some(error) = &s.error {
                output.events.push((
                    "module.notice".into(),
                    json!(format!("Durable scheduled tasks paused: {error}")),
                ));
            }
        }
        output
            .events
            .push((STATUS.into(), json!(s.status(at, &System))));
        drop(s);
        if start {
            self.start();
        }
        Ok(output)
    }

    fn continuation(&self) -> Option<Continuation> {
        self.lock().ok()?.due(now(), &System)
    }

    fn accepts(&self, next: &Continuation) -> bool {
        self.continuation().is_some_and(|current| {
            current.generation == next.generation && current.identity == next.identity
        })
    }
}

struct LoopCommand {
    inner: Arc<Loop>,
    name: &'static str,
}

const USAGE: &str = "Usage: /loop [interval] <prompt>

Runs the prompt now and then every interval while this session is open.
Intervals: Ns, Nm, Nh, Nd (default 10m), from 1 minute to 31 days.
  /loop 5m check the deploy
  /loop check the deploy every 20m";

#[async_trait]
impl Command for LoopCommand {
    fn info(&self) -> CommandInfo {
        let (description, hint) = match self.name {
            "loop" => (
                "Run a prompt now and then on an interval (default 10m)",
                Some("[interval] <prompt>"),
            ),
            "cron-list" => ("List scheduled tasks", None),
            _ => ("Cancel a scheduled task", Some("<id>")),
        };
        CommandInfo {
            name: self.name.into(),
            description: description.into(),
            argument_hint: hint.map(Into::into),
        }
    }

    fn allows_busy(&self, _: &str) -> bool {
        true
    }

    async fn run(&self, arguments: &str, cancel: CancellationToken) -> Result<String, ToolError> {
        match self.name {
            "loop" => {
                let request = cron::request(arguments).ok_or_else(|| ToolError(USAGE.into()))?;
                let interval = cron::interval(request.amount, request.unit)
                    .ok_or_else(|| ToolError(USAGE.into()))?;
                let created = self
                    .inner
                    .create(
                        Spec {
                            cron: interval.cron.clone(),
                            prompt: request.prompt.into(),
                            recurring: true,
                            durable: false,
                            now: true,
                        },
                        cancel,
                    )
                    .await
                    .map_err(ToolError)?;
                let every = cron::period(interval.minutes);
                let requested = format!("{}{}", request.amount, request.unit);
                let rounded = if cron::seconds(request.amount, request.unit)
                    == interval.minutes.checked_mul(60)
                {
                    String::new()
                } else {
                    format!(", {requested} rounded to the nearest even period")
                };
                let month = if interval.minutes > 1440 {
                    " Day steps restart each month, so a gap at month end can be shorter."
                } else {
                    ""
                };
                Ok(format!(
                    "Scheduled {}: {every} (`{}` local time{rounded}).{month} Session-only; expires after 7 days. Cancel with /cron-delete {}. Running it now.",
                    created.task.id, interval.cron, created.task.id
                ))
            }
            "cron-list" => {
                let listed = self.inner.list().await.map_err(ToolError)?;
                Ok(if listed.is_empty() {
                    "No scheduled tasks.".into()
                } else {
                    listed
                        .iter()
                        .map(|l| l.lines(&System))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
            }
            _ => {
                let id = arguments.trim();
                if id.is_empty() {
                    return Err(ToolError("Usage: /cron-delete <id>".into()));
                }
                self.inner.delete(id, cancel).await.map_err(ToolError)
            }
        }
    }
}

struct CronTool {
    inner: Arc<Loop>,
    spec: ToolSpec,
}

#[async_trait]
impl Tool for CronTool {
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }

    fn source(&self) -> Option<String> {
        Some("Built-in tools".into())
    }

    fn concurrency(&self, _: &Value) -> Concurrency {
        if self.spec.name == "cron_list" {
            Concurrency::Shared
        } else {
            Concurrency::Exclusive
        }
    }

    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let arguments = &call.arguments;
        let text = |key: &str| arguments[key].as_str().unwrap_or_default().to_owned();
        match self.spec.name.as_str() {
            "cron_create" => {
                let created = self
                    .inner
                    .create(
                        Spec {
                            cron: text("cron"),
                            prompt: text("prompt"),
                            recurring: arguments["recurring"].as_bool().unwrap_or(true),
                            durable: arguments["durable"].as_bool().unwrap_or(false),
                            now: false,
                        },
                        ctx.cancel,
                    )
                    .await
                    .map_err(ToolError)?;
                let place = if created.durable {
                    format!("Saved to {}", store::FILE)
                } else {
                    "Session-only".into()
                };
                let lifetime = if created.task.recurring {
                    "Expires after 7 days; use cron_delete to cancel sooner."
                } else {
                    "Runs once, then is deleted."
                };
                Ok(ToolOutput {
                    structured_content: Some(created.json(&System)),
                    ..ToolOutput::text(format!(
                        "Scheduled {} {} ({}), next run {}. {place}. {lifetime}",
                        if created.task.recurring {
                            "recurring task"
                        } else {
                            "one-shot task"
                        },
                        created.task.id,
                        cron::describe(&created.task.cron),
                        created
                            .due
                            .map_or_else(|| "never".into(), |at| cron::format(at, &System)),
                    ))
                })
            }
            "cron_list" => {
                let listed = self.inner.list().await.map_err(ToolError)?;
                let tasks: Vec<_> = listed.iter().map(|l| l.json(&System)).collect();
                Ok(ToolOutput {
                    structured_content: Some(json!({ "tasks": tasks })),
                    ..ToolOutput::text(if listed.is_empty() {
                        "No scheduled tasks.".into()
                    } else {
                        tiechui_plugin_api::serde_json::to_string_pretty(&tasks).unwrap_or_default()
                    })
                })
            }
            _ => self
                .inner
                .delete(&text("id"), ctx.cancel)
                .await
                .map(ToolOutput::text)
                .map_err(ToolError),
        }
    }
}

#[cfg(test)]
mod tests;
