//! `chui app-server`: sessions for desktop clients, over JSON-RPC 2.0 on stdio with one
//! message per line. Each session runs as in the TUI, with approvals sent to the client.

mod projection;
#[cfg(feature = "workspace")]
mod workspace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};
use tiechui_config::TrustStore;
use tiechui_model::Role;
use tiechui_protocol::app::*;
use tiechui_protocol::rpc::{self, Id, JsonRpcMessage};
use tiechui_protocol::{
    Event, EventKind, Op, PERMISSION_MODE_EVENT, SessionEpoch, Submission, SubmissionId, ThreadId,
};
use tiechui_session::{Entry, EntryKind, SessionStore};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::exec::{Open, Options, Prepared, locations, prepare};
use projection::Projection;

const MAX_FRAME: u64 = 16 << 20;

type Reply = Result<Value, rpc::Error>;

struct Live {
    path: PathBuf,
    cwd: PathBuf,
    submissions: mpsc::Sender<Submission>,
    shutdown: CancellationToken,
    projection: Arc<Mutex<Projection>>,
    /// The epoch of the latest `ThreadStarted`.
    started: watch::Receiver<Option<SessionEpoch>>,
    pump: Mutex<Option<JoinHandle<()>>>,
}

struct Server {
    home: PathBuf,
    out: mpsc::UnboundedSender<String>,
    sessions: Mutex<HashMap<ThreadId, Arc<Live>>>,
    schedules: tiechui_module_loop::Schedules,
    next: AtomicU64,
    #[cfg(feature = "workspace")]
    workspace: Arc<tiechui_workspace::Workspace>,
}

pub(crate) async fn run() -> i32 {
    let home = match locations(&Options::default()) {
        Ok((_, _, _, home)) => home,
        Err((code, message)) => {
            eprintln!("chui: {message}");
            return code;
        }
    };
    let (out, mut lines) = mpsc::unbounded_channel::<String>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(mut line) = lines.recv().await {
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let server = Arc::new(Server {
        home,
        #[cfg(feature = "workspace")]
        workspace: workspace::new(out.clone()),
        out,
        sessions: Mutex::default(),
        schedules: Default::default(),
        next: AtomicU64::new(1),
    });
    let mut stdin = BufReader::new(tokio::io::stdin());
    let mut line = Vec::new();
    let code = loop {
        line.clear();
        match (&mut stdin)
            .take(MAX_FRAME + 1)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) => break 0,
            Ok(_) if line.len() as u64 > MAX_FRAME => {
                eprintln!("chui app-server: a message exceeds 16 MiB");
                break 1;
            }
            Ok(_) => server.receive(&line),
            Err(error) => {
                eprintln!("chui app-server: {error}");
                break 1;
            }
        }
    };
    let open: Vec<_> = lock(&server.sessions).keys().cloned().collect();
    for thread in open {
        server.close(&thread).await;
    }
    #[cfg(feature = "workspace")]
    server.workspace.close();
    writer.abort();
    code
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

fn failure(code: i64, message: impl ToString) -> rpc::Error {
    rpc::Error {
        code,
        message: message.to_string(),
        data: None,
    }
}

fn invalid(message: impl ToString) -> rpc::Error {
    failure(-32602, message)
}

fn internal(message: impl ToString) -> rpc::Error {
    failure(-32000, message)
}

fn parse<T: DeserializeOwned>(params: Value) -> Result<T, rpc::Error> {
    serde_json::from_value(params).map_err(invalid)
}

fn value(result: impl Serialize) -> Reply {
    serde_json::to_value(result).map_err(internal)
}

async fn blocking<T: Send + 'static>(
    work: impl FnOnce(&CancellationToken) -> Result<T, String> + Send + 'static,
) -> Result<T, rpc::Error> {
    tokio::task::spawn_blocking(move || work(&CancellationToken::new()))
        .await
        .map_err(internal)?
        .map_err(internal)
}

impl Server {
    fn send(&self, message: Value) {
        let _ = self.out.send(message.to_string());
    }

    fn notify(&self, method: &str, params: impl Serialize) {
        self.send(json!({ "jsonrpc": "2.0", "method": method, "params": params }));
    }

    fn respond(&self, id: Id, reply: Reply) {
        self.send(match reply {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
        });
    }

    fn receive(self: &Arc<Self>, line: &[u8]) {
        match serde_json::from_slice::<JsonRpcMessage>(line) {
            Ok(JsonRpcMessage::Request(request)) => {
                let server = self.clone();
                let params = request.params.map_or(Value::Null, |params| json!(params));
                tokio::spawn(async move {
                    server.dispatch(request.id, &request.method, params).await;
                });
            }
            // This server sends no requests, so responses and notifications are unexpected.
            Ok(_) => {}
            Err(error) => self.respond(Id::Null, Err(failure(-32700, error))),
        }
    }

    async fn dispatch(self: &Arc<Self>, id: Id, method: &str, params: Value) {
        let reply = match method {
            "initialize" => self.initialize(params),
            "session/list" => self.list(params).await,
            "session/open" | "session/fork" | "session/reload" | "session/snapshot" => {
                let opened = match method {
                    "session/open" => self.open(params).await,
                    "session/fork" => self.fork(params).await,
                    "session/reload" => self.reload(params).await,
                    _ => parse::<ThreadParams>(params).and_then(|p| self.live(&p.thread_id)),
                };
                match opened {
                    Ok(live) => return self.snapshot(id, &live),
                    Err(error) => Err(error),
                }
            }
            "session/submit" => match parse::<SubmitParams>(params) {
                Ok(p) => self.submit(&p.thread_id, p.op).await.and_then(value),
                Err(error) => Err(error),
            },
            "session/branch" => self.branch(params).await,
            "session/close" => match parse::<ThreadParams>(params) {
                Ok(p) => {
                    self.close(&p.thread_id).await;
                    Ok(Value::Null)
                }
                Err(error) => Err(error),
            },
            "session/rename" => self.rename(params).await,
            "session/archive" => self.archive(params).await,
            "session/trash" => self.trash(params).await,
            "trust" => self.trust(params).await,
            #[cfg(feature = "workspace")]
            _ if workspace::handles(method) => {
                workspace::dispatch(&self.workspace, method, params).await
            }
            _ => Err(failure(-32601, format!("unknown method {method}"))),
        };
        self.respond(id, reply);
    }

    fn initialize(&self, params: Value) -> Reply {
        let params: InitializeParams = parse(params)?;
        if params.protocol_version != APP_PROTOCOL_VERSION {
            return Err(invalid(format!(
                "protocol version {} is not {APP_PROTOCOL_VERSION}",
                params.protocol_version
            )));
        }
        value(InitializeResult {
            protocol_version: APP_PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").into(),
            home: self.home.display().to_string(),
            capabilities: if cfg!(feature = "workspace") {
                vec!["workspace".into()]
            } else {
                vec![]
            },
        })
    }

    fn live(&self, thread: &ThreadId) -> Result<Arc<Live>, rpc::Error> {
        lock(&self.sessions)
            .get(thread)
            .cloned()
            .ok_or_else(|| invalid(format!("session {} is not open", thread.0)))
    }

    fn by_path(&self, path: &Path) -> Option<(ThreadId, Arc<Live>)> {
        lock(&self.sessions)
            .iter()
            .find(|(_, live)| live.path == path)
            .map(|(thread, live)| (thread.clone(), live.clone()))
    }

    /// Responds while holding the projection, so later events follow the snapshot.
    fn snapshot(&self, id: Id, live: &Live) {
        let projection = lock(&live.projection);
        let thread_id = match projection.snapshot().first() {
            Some(event) => event.thread_id.clone(),
            None => return self.respond(id, Err(internal("session has not started"))),
        };
        self.respond(
            id,
            value(Snapshot {
                thread_id,
                path: live.path.display().to_string(),
                cwd: live.cwd.display().to_string(),
                seq: projection.seq,
                events: projection.snapshot(),
            }),
        );
    }

    async fn list(&self, params: Value) -> Reply {
        let params: SessionListParams = parse(params)?;
        let home = self.home.clone();
        let cwd = PathBuf::from(&params.cwd);
        let (summaries, _) = blocking(move |cancel| {
            tiechui_session::summaries(&home, Some(&cwd), cancel).map_err(|e| e.to_string())
        })
        .await?;
        let sessions = lock(&self.sessions);
        let mut listed: Vec<_> = summaries
            .into_iter()
            .filter(|s| params.archived || s.meta.archived_at.is_none())
            .map(|s| {
                let thread_id = ThreadId(s.header.id);
                SessionSummary {
                    live: sessions
                        .get(&thread_id)
                        .map(|live| lock(&live.projection).state()),
                    thread_id,
                    path: s.path.display().to_string(),
                    cwd: s.header.cwd.display().to_string(),
                    created_at: s.header.created_at,
                    modified: s.modified,
                    first_message: s.first_message,
                    title: s.meta.title,
                    archived_at: s.meta.archived_at,
                    parent: s.header.parent_session.map(|p| p.display().to_string()),
                }
            })
            .collect();
        listed.sort_by_key(|s| std::cmp::Reverse(s.modified));
        value(listed)
    }

    async fn open(self: &Arc<Self>, params: Value) -> Result<Arc<Live>, rpc::Error> {
        let params: SessionOpenParams = parse(params)?;
        let thinking = params
            .thinking
            .map(|level| json!(level).as_str().unwrap_or_default().to_owned());
        match (params.path, params.cwd) {
            (Some(path), None) => {
                let path = PathBuf::from(path);
                let live = match self.by_path(&path) {
                    Some((_, live)) => live,
                    None => {
                        let options = Options {
                            mode: params.mode,
                            ..Default::default()
                        };
                        self.start(options, Open::Path(path), SessionEpoch(0), None)
                            .await?
                            .1
                    }
                };
                // A resumed session keeps its own model unless the client asks otherwise.
                let thread = self.thread_of(&live)?;
                if let Some(model) = params.model {
                    let model = crate::exec::parse_model(&model).map_err(|(_, m)| invalid(m))?;
                    self.submit(
                        &thread,
                        Op::SetModel {
                            provider: model.provider,
                            model: model.model.0,
                        },
                    )
                    .await?;
                }
                if let Some(level) = params.thinking {
                    self.submit(&thread, Op::SetThinking { level }).await?;
                }
                Ok(live)
            }
            (None, Some(cwd)) => {
                let options = Options {
                    model: params.model,
                    thinking,
                    mode: params.mode,
                    ..Default::default()
                };
                Ok(self
                    .start(options, Open::New(cwd.into()), SessionEpoch(0), None)
                    .await?
                    .1)
            }
            _ => Err(invalid("give exactly one of path and cwd")),
        }
    }

    fn thread_of(&self, live: &Arc<Live>) -> Result<ThreadId, rpc::Error> {
        lock(&self.sessions)
            .iter()
            .find(|(_, open)| Arc::ptr_eq(open, live))
            .map(|(thread, _)| thread.clone())
            .ok_or_else(|| internal("session ended"))
    }

    /// Opens a session and waits for its thread to start.
    async fn start(
        self: &Arc<Self>,
        options: Options,
        open: Open,
        epoch: SessionEpoch,
        projection: Option<Arc<Mutex<Projection>>>,
    ) -> Result<(ThreadId, Arc<Live>), rpc::Error> {
        let shutdown = CancellationToken::new();
        let mut warnings = Vec::new();
        let prepared = prepare(
            &options,
            shutdown.clone(),
            true,
            open,
            epoch,
            &self.schedules,
            &mut warnings,
        )
        .await;
        for warning in warnings {
            eprintln!("{warning}");
        }
        let Prepared {
            handle,
            id,
            cwd,
            path,
            ..
        } = prepared.map_err(|(_, message)| internal(message))?;
        let thread = ThreadId(id);
        let projection = projection.unwrap_or_default();
        lock(&projection).epoch = epoch;
        let (ready, mut started) = watch::channel(None);
        let live = Arc::new(Live {
            path,
            cwd,
            submissions: handle.submissions,
            shutdown: shutdown.clone(),
            projection: projection.clone(),
            started: started.clone(),
            pump: Mutex::new(None),
        });
        lock(&self.sessions).insert(thread.clone(), live.clone());
        let pump = tokio::spawn(self.clone().pump(
            thread.clone(),
            handle.events,
            handle.task,
            projection,
            shutdown,
            ready,
        ));
        *lock(&live.pump) = Some(pump);
        if started.wait_for(Option::is_some).await.is_err() {
            lock(&self.sessions).remove(&thread);
            return Err(internal("the session ended before it started"));
        }
        Ok((thread, live))
    }

    async fn pump(
        self: Arc<Self>,
        thread: ThreadId,
        mut events: broadcast::Receiver<Event>,
        task: JoinHandle<Result<(), String>>,
        projection: Arc<Mutex<Projection>>,
        shutdown: CancellationToken,
        ready: watch::Sender<Option<SessionEpoch>>,
    ) {
        loop {
            match events.recv().await {
                Ok(event) => {
                    let mut projection = lock(&projection);
                    let seq = projection.apply(&event);
                    let epoch = matches!(event.event, EventKind::ThreadStarted { .. })
                        .then_some(event.epoch);
                    self.notify("session/event", SessionEvent { seq, event });
                    if epoch.is_some() {
                        ready.send_replace(epoch);
                    }
                }
                Err(broadcast::error::RecvError::Lagged(count)) => {
                    eprintln!("chui app-server: {}: {count} events were dropped", thread.0);
                }
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
        let error = match task.await {
            Ok(Ok(())) => None,
            Ok(Err(error)) => Some(error),
            Err(error) => Some(error.to_string()),
        };
        if !shutdown.is_cancelled() {
            lock(&self.sessions).remove(&thread);
            self.notify(
                "session/closed",
                SessionClosed {
                    thread_id: thread,
                    error,
                },
            );
        }
    }

    async fn submit(&self, thread: &ThreadId, op: Op) -> Result<SubmitResult, rpc::Error> {
        let live = self.live(thread)?;
        let id = SubmissionId(format!("app-{}", self.next.fetch_add(1, Ordering::Relaxed)));
        let epoch = lock(&live.projection).epoch;
        live.submissions
            .send(Submission {
                id: id.clone(),
                thread_id: thread.clone(),
                epoch,
                op,
            })
            .await
            .map_err(|_| internal("session ended"))?;
        Ok(SubmitResult { submission_id: id })
    }

    async fn close(&self, thread: &ThreadId) {
        let Some(live) = lock(&self.sessions).remove(thread) else {
            return;
        };
        live.shutdown.cancel();
        let pump = lock(&live.pump).take();
        if let Some(pump) = pump {
            let _ = pump.await;
        }
    }

    /// Starts the session again from its file, so configuration and modules reload.
    async fn reload(self: &Arc<Self>, params: Value) -> Result<Arc<Live>, rpc::Error> {
        let thread = parse::<ThreadParams>(params)?.thread_id;
        let live = self.live(&thread)?;
        let options = Options {
            mode: mode(&live),
            ..Default::default()
        };
        let epoch = SessionEpoch(lock(&live.projection).epoch.0 + 1);
        self.close(&thread).await;
        Ok(self
            .start(
                options,
                Open::Path(live.path.clone()),
                epoch,
                Some(live.projection.clone()),
            )
            .await?
            .1)
    }

    /// Copies the branch through the named turn into a new session and opens it.
    async fn fork(self: &Arc<Self>, params: Value) -> Result<Arc<Live>, rpc::Error> {
        let params: TurnParams = parse(params)?;
        let live = self.live(&params.thread_id)?;
        let path = live.path.clone();
        let forked = blocking(move |cancel| {
            let store = SessionStore::open_read_only(&path, cancel).map_err(|e| e.to_string())?;
            let end = turn_end(&store, &params.item_id)?;
            let fork = store.fork(&end, cancel).map_err(|e| e.to_string())?;
            Ok(fork.path().to_owned())
        })
        .await?;
        let options = Options {
            mode: mode(&live),
            ..Default::default()
        };
        Ok(self
            .start(options, Open::Path(forked), SessionEpoch(0), None)
            .await?
            .1)
    }

    /// Moves the session to just before the named turn and answers once the replay has been
    /// sent, so the next input carries the new epoch and replaces the turn.
    async fn branch(&self, params: Value) -> Reply {
        let params: TurnParams = parse(params)?;
        let live = self.live(&params.thread_id)?;
        let path = live.path.clone();
        let before = lock(&live.projection).epoch;
        let item = params.item_id.clone();
        let target = blocking(move |cancel| {
            let store = SessionStore::open_read_only(&path, cancel).map_err(|e| e.to_string())?;
            let entry = user_entry(&store, &item)?;
            Ok(entry
                .parent_id
                .clone()
                .unwrap_or_else(|| store.header().id.clone()))
        })
        .await?;
        let submitted = self
            .submit(&params.thread_id, Op::Branch { entry_id: target })
            .await?;
        live.started
            .clone()
            .wait_for(|epoch| epoch.is_some_and(|epoch| epoch.0 > before.0))
            .await
            .map_err(|_| internal("session ended"))?;
        value(submitted)
    }

    async fn rename(&self, params: Value) -> Reply {
        let params: RenameParams = parse(params)?;
        update_meta(params.path, move |meta| meta.title = params.title).await
    }

    async fn archive(&self, params: Value) -> Reply {
        let params: ArchiveParams = parse(params)?;
        let at = params.archived.then(tiechui_session::now_ms);
        update_meta(params.path, move |meta| meta.archived_at = at).await
    }

    async fn trash(&self, params: Value) -> Reply {
        let path = PathBuf::from(parse::<PathParams>(params)?.path);
        if let Some((thread, _)) = self.by_path(&path) {
            self.close(&thread).await;
        }
        let home = self.home.clone();
        let moved =
            blocking(move |_| tiechui_session::trash(&home, &path).map_err(|e| e.to_string()))
                .await?;
        value(PathParams {
            path: moved.display().to_string(),
        })
    }

    async fn trust(&self, params: Value) -> Reply {
        let params: TrustParams = parse(params)?;
        let home = self.home.clone();
        let allowed = blocking(move |cancel| {
            let cwd = PathBuf::from(&params.cwd)
                .canonicalize()
                .map_err(|e| format!("{}: {e}", params.cwd))?;
            let store = TrustStore::new(&home);
            if let Some(allow) = params.allow {
                store
                    .set(&cwd, Some(allow), cancel)
                    .map_err(|e| e.to_string())?;
            }
            store.get(&cwd, cancel).map_err(|e| e.to_string())
        })
        .await?;
        value(TrustResult { allowed })
    }
}

/// The permission mode the session last reported.
fn mode(live: &Live) -> Option<String> {
    lock(&live.projection)
        .snapshot()
        .into_iter()
        .find_map(|event| match event.event {
            EventKind::Extension { name, data } if name == PERMISSION_MODE_EVENT => {
                data.as_str().map(str::to_owned)
            }
            _ => None,
        })
}

async fn update_meta(
    path: String,
    change: impl FnOnce(&mut tiechui_session::Meta) + Send + 'static,
) -> Reply {
    blocking(move |_| {
        let path = Path::new(&path);
        let mut meta = tiechui_session::meta(path).map_err(|e| e.to_string())?;
        change(&mut meta);
        tiechui_session::save_meta(path, &meta).map_err(|e| e.to_string())
    })
    .await?;
    Ok(Value::Null)
}

/// The user message entry behind a turn's user message item: live items carry the entry
/// id, replayed ones append the content index.
fn user_entry<'a>(store: &'a SessionStore, item: &str) -> Result<&'a Entry, String> {
    let found = |id: &str| {
        store
            .path_to_root(id)
            .ok()
            .and_then(|path| path.last().copied())
    };
    let entry = found(item)
        .or_else(|| item.rsplit_once('-').and_then(|(id, _)| found(id)))
        .ok_or_else(|| format!("unknown message {item}"))?;
    if !starts_turn(entry) {
        return Err(format!("{item} is not a user message"));
    }
    Ok(entry)
}

/// The last entry of the turn the item opens, on the current branch.
fn turn_end(store: &SessionStore, item: &str) -> Result<String, String> {
    let start = user_entry(store, item)?.id.clone();
    let branch = store
        .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
        .map_err(|e| e.to_string())?;
    let index = branch
        .iter()
        .position(|entry| entry.id == start)
        .ok_or_else(|| format!("{item} is not on the current branch"))?;
    let end = branch[index + 1..]
        .iter()
        .take_while(|entry| !starts_turn(entry))
        .last()
        .unwrap_or(&branch[index]);
    Ok(end.id.clone())
}

/// As replayed threads divide turns.
fn starts_turn(entry: &Entry) -> bool {
    matches!(&entry.kind, EntryKind::Message { message, .. } if message.role == Role::User)
}
