//! `chui app-server`: sessions for desktop clients, over JSON-RPC 2.0 on stdio with one
//! message per line. Each session runs as in the TUI, with approvals sent to the client.

mod agents;
mod hooks;
mod mcp;
mod projection;
mod prompts;
mod providers;
mod skills;
mod textgen;
#[cfg(all(test, feature = "workspace"))]
mod textgen_tests;
mod turns;
#[cfg(feature = "workspace")]
mod workspace;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

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
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Notify, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::exec::{Open, Options, Prepared, locations, prepare};
use projection::Projection;
pub(crate) use textgen::watch_session_title;

const MAX_FRAME: u64 = 16 << 20;
#[cfg(unix)]
const DEFAULT_DAEMON_IDLE: Duration = Duration::from_secs(30 * 60);

type Reply = Result<Value, rpc::Error>;
type ClientOut = mpsc::UnboundedSender<String>;

struct Live {
    path: PathBuf,
    cwd: PathBuf,
    submissions: mpsc::Sender<Submission>,
    shutdown: CancellationToken,
    projection: Arc<Mutex<Projection>>,
    /// The epoch of the latest `ThreadStarted`.
    started: watch::Receiver<Option<SessionEpoch>>,
    pump: Mutex<Option<JoinHandle<()>>>,
    /// The configuration changed since the session started.
    stale: AtomicBool,
}

struct Server {
    home: PathBuf,
    clients: Arc<Mutex<HashMap<u64, ClientOut>>>,
    next_client: AtomicU64,
    sessions: Mutex<HashMap<ThreadId, Arc<Live>>>,
    reloading: tokio::sync::Mutex<()>,
    config_gen: AtomicU64,
    schedules: tiechui_module_loop::Schedules,
    next: AtomicU64,
    /// Wakes the archiving of inactive sessions early.
    sweep: Notify,
    activity: Arc<Notify>,
    activity_seq: Arc<AtomicU64>,
    shutdown: CancellationToken,
    cancels: Mutex<textgen::CancelRegistry>,
    title_tasks: Mutex<Vec<JoinHandle<()>>>,
    #[cfg(feature = "workspace")]
    workspace: Arc<tiechui_workspace::Workspace>,
}

pub(crate) async fn run(args: &clap::ArgMatches) -> i32 {
    if let Some(raw) = args.get_one::<String>("connect") {
        return connect_socket(&expand_socket_path(raw)).await;
    }
    if let Some(raw) = args.get_one::<String>("socket") {
        return serve_socket(&expand_socket_path(raw)).await;
    }
    serve_stdio().await
}

fn expand_socket_path(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/")
        && let Some(home) = tiechui_config::user_home()
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

async fn serve_stdio() -> i32 {
    let home = match locations(&Options::default()) {
        Ok((_, _, _, home)) => home,
        Err((code, message)) => {
            eprintln!("chui: {message}");
            return code;
        }
    };
    let server = Server::new(home);
    let (client_id, out, mut lines) = server.add_client();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(mut line) = lines.recv().await {
            line.push('\n');
            if stdout.write_all(line.as_bytes()).await.is_err() || stdout.flush().await.is_err() {
                break;
            }
        }
    });
    let archiving = tokio::spawn(server.clone().archive_inactive());
    let code = read_client_lines(&server, client_id, &out, tokio::io::stdin()).await;
    server.remove_client(client_id);
    drop(out);
    server.shutdown_all().await;
    archiving.abort();
    writer.abort();
    code
}

async fn read_client_lines<R: AsyncRead + Unpin>(
    server: &Arc<Server>,
    client_id: u64,
    out: &ClientOut,
    reader: R,
) -> i32 {
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    loop {
        line.clear();
        match (&mut reader)
            .take(MAX_FRAME + 1)
            .read_until(b'\n', &mut line)
            .await
        {
            Ok(0) => break 0,
            Ok(_) if line.len() as u64 > MAX_FRAME => {
                eprintln!("chui app-server: a message exceeds 16 MiB");
                break 1;
            }
            Ok(_) => server.receive(client_id, out, &line),
            Err(error) => {
                eprintln!("chui app-server: {error}");
                break 1;
            }
        }
    }
}

#[cfg(not(unix))]
async fn connect_socket(path: &Path) -> i32 {
    eprintln!(
        "chui app-server: {}: Unix domain sockets are not supported on this platform",
        path.display()
    );
    1
}

#[cfg(not(unix))]
async fn serve_socket(path: &Path) -> i32 {
    eprintln!(
        "chui app-server: {}: Unix domain sockets are not supported on this platform",
        path.display()
    );
    1
}

#[cfg(unix)]
async fn connect_socket(path: &Path) -> i32 {
    let stream = match tokio::net::UnixStream::connect(path).await {
        Ok(stream) => stream,
        Err(error) => {
            eprintln!("chui app-server: {}: {error}", path.display());
            return 1;
        }
    };
    let (mut sock_read, mut sock_write) = stream.into_split();
    let stdin_failed = Arc::new(AtomicBool::new(false));
    let stdin_failed_writer = stdin_failed.clone();
    let to_sock = tokio::spawn(async move {
        let mut stdin = tokio::io::stdin();
        let mut buf = [0u8; 16384];
        loop {
            match stdin.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    if sock_write.write_all(&buf[..n]).await.is_err()
                        || sock_write.flush().await.is_err()
                    {
                        break;
                    }
                }
                Err(error) => {
                    eprintln!("chui app-server: {error}");
                    stdin_failed_writer.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }
        let _ = sock_write.shutdown().await;
    });

    let mut stdout = tokio::io::stdout();
    let mut buf = [0u8; 16384];
    let mut code = 0;
    loop {
        match sock_read.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if stdout.write_all(&buf[..n]).await.is_err() || stdout.flush().await.is_err() {
                    break;
                }
            }
            Err(error) => {
                eprintln!("chui app-server: {}: {error}", path.display());
                code = 1;
                break;
            }
        }
    }
    to_sock.abort();
    if code == 0 && stdin_failed.load(Ordering::Relaxed) {
        1
    } else {
        code
    }
}

#[cfg(unix)]
async fn serve_socket(path: &Path) -> i32 {
    use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};

    let home = match locations(&Options::default()) {
        Ok((_, _, _, home)) => home,
        Err((code, message)) => {
            eprintln!("chui: {message}");
            return code;
        }
    };
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
        && !parent.exists()
        && let Err(error) = std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(parent)
    {
        eprintln!("chui app-server: {}: {error}", parent.display());
        return 1;
    }

    if tokio::net::UnixStream::connect(path).await.is_ok() {
        return 0;
    }
    if let Ok(meta) = std::fs::symlink_metadata(path) {
        if !meta.file_type().is_socket() {
            eprintln!(
                "chui app-server: {}: file exists and is not a socket",
                path.display()
            );
            return 1;
        }
        if let Err(error) = std::fs::remove_file(path)
            && error.kind() != std::io::ErrorKind::NotFound
        {
            eprintln!("chui app-server: {}: {error}", path.display());
            return 1;
        }
    }

    let listener = match tokio::net::UnixListener::bind(path) {
        Ok(listener) => listener,
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => {
            tokio::time::sleep(Duration::from_millis(25)).await;
            if tokio::net::UnixStream::connect(path).await.is_ok() {
                return 0;
            }
            eprintln!("chui app-server: {}: {error}", path.display());
            return 1;
        }
        Err(error) => {
            eprintln!("chui app-server: {}: {error}", path.display());
            return 1;
        }
    };
    if let Err(error) = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)) {
        let _ = std::fs::remove_file(path);
        eprintln!("chui app-server: {}: {error}", path.display());
        return 1;
    }
    let bound_id = std::fs::symlink_metadata(path)
        .ok()
        .map(|meta| (meta.dev(), meta.ino()));

    let idle_timeout = std::env::var("CHUI_DAEMON_IDLE_MS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .map_or(DEFAULT_DAEMON_IDLE, Duration::from_millis);
    let server = Server::new(home);
    let archiving = tokio::spawn(server.clone().archive_inactive());
    let mut sigterm =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok();
    let mut idle_state: Option<(u64, tokio::time::Instant)> = None;

    loop {
        let notified = server.activity.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();

        let seq_now = server.activity_seq.load(Ordering::Acquire);
        if server.is_idle() {
            match idle_state {
                Some((armed_seq, _)) if armed_seq == seq_now => {}
                _ => {
                    idle_state = Some((seq_now, tokio::time::Instant::now() + idle_timeout));
                }
            }
        } else {
            idle_state = None;
        }

        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        idle_state = None;
                        let (client_id, out, mut lines) = server.add_client();
                        let (read_half, mut write_half) = stream.into_split();
                        let server_writer = server.clone();
                        let writer = tokio::spawn(async move {
                            while let Some(mut line) = lines.recv().await {
                                line.push('\n');
                                if write_half.write_all(line.as_bytes()).await.is_err()
                                    || write_half.flush().await.is_err()
                                {
                                    server_writer.remove_client(client_id);
                                    break;
                                }
                            }
                            let _ = write_half.shutdown().await;
                        });
                        let server_reader = server.clone();
                        tokio::spawn(async move {
                            let _ =
                                read_client_lines(&server_reader, client_id, &out, read_half).await;
                            server_reader.remove_client(client_id);
                            drop(out);
                            let _ = writer.await;
                            server_reader.touch();
                        });
                    }
                    Err(error) => {
                        eprintln!("chui app-server: accept: {error}");
                        tokio::time::sleep(Duration::from_millis(50)).await;
                    }
                }
            }
            () = &mut notified => {}
            () = async {
                match idle_state {
                    Some((_, deadline)) => tokio::time::sleep_until(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            } => {
                if server.is_idle()
                    && idle_state.is_some_and(|(armed_seq, _)| {
                        armed_seq == server.activity_seq.load(Ordering::Acquire)
                    })
                {
                    break;
                }
            }
            _ = tokio::signal::ctrl_c() => break,
            _ = async {
                match sigterm.as_mut() {
                    Some(sig) => {
                        let _ = sig.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => break,
        }
    }

    if let Some(expected) = bound_id
        && let Ok(meta) = std::fs::symlink_metadata(path)
        && (meta.dev(), meta.ino()) == expected
    {
        let _ = std::fs::remove_file(path);
    }
    server.shutdown_all().await;
    archiving.abort();
    0
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

fn broadcast(clients: &Mutex<HashMap<u64, ClientOut>>, line: &str) {
    for out in lock(clients).values() {
        let _ = out.send(line.to_owned());
    }
}

fn respond(out: &ClientOut, id: Id, reply: Reply) {
    let message = match reply {
        Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
        Err(error) => json!({ "jsonrpc": "2.0", "id": id, "error": error }),
    };
    let _ = out.send(message.to_string());
}

impl Server {
    fn new(home: PathBuf) -> Arc<Self> {
        let clients = Arc::new(Mutex::new(HashMap::<u64, ClientOut>::new()));
        let activity = Arc::new(Notify::new());
        let activity_seq = Arc::new(AtomicU64::new(0));
        #[cfg(feature = "workspace")]
        let workspace = {
            let ws_clients = clients.clone();
            let ws_activity = activity.clone();
            let ws_seq = activity_seq.clone();
            workspace::new(Arc::new(move |method, params| {
                let line =
                    json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
                broadcast(&ws_clients, &line);
                if matches!(method, "terminal/output" | "terminal/exit") {
                    ws_seq.fetch_add(1, Ordering::Release);
                    ws_activity.notify_one();
                }
            }))
        };
        Arc::new(Self {
            home,
            clients,
            next_client: AtomicU64::new(1),
            sessions: Mutex::default(),
            reloading: tokio::sync::Mutex::new(()),
            config_gen: AtomicU64::new(0),
            schedules: Default::default(),
            next: AtomicU64::new(1),
            sweep: Notify::new(),
            activity,
            activity_seq,
            shutdown: CancellationToken::new(),
            cancels: Mutex::default(),
            title_tasks: Mutex::default(),
            #[cfg(feature = "workspace")]
            workspace,
        })
    }

    fn touch(&self) {
        self.activity_seq.fetch_add(1, Ordering::Release);
        self.activity.notify_one();
    }

    fn add_client(&self) -> (u64, ClientOut, mpsc::UnboundedReceiver<String>) {
        let id = self.next_client.fetch_add(1, Ordering::Relaxed);
        let (out, lines) = mpsc::unbounded_channel();
        lock(&self.clients).insert(id, out.clone());
        self.touch();
        (id, out, lines)
    }

    fn remove_client(&self, id: u64) {
        if lock(&self.clients).remove(&id).is_some() {
            lock(&self.cancels).remove_client(id);
            #[cfg(feature = "workspace")]
            self.workspace.remove_client(id);
            self.touch();
        }
    }

    fn busy_sessions(&self) -> usize {
        lock(&self.sessions)
            .values()
            .filter(|live| lock(&live.projection).busy())
            .count()
    }

    fn alive_terminals(&self) -> usize {
        #[cfg(feature = "workspace")]
        {
            self.workspace.alive_terminals()
        }
        #[cfg(not(feature = "workspace"))]
        {
            0
        }
    }

    fn daemon_status(&self) -> DaemonStatus {
        DaemonStatus {
            pid: std::process::id(),
            server_version: env!("CARGO_PKG_VERSION").into(),
            protocol_version: APP_PROTOCOL_VERSION,
            connected_clients: lock(&self.clients).len(),
            busy_sessions: self.busy_sessions(),
            alive_terminals: self.alive_terminals(),
        }
    }

    #[cfg(unix)]
    fn is_idle(&self) -> bool {
        lock(&self.clients).is_empty() && self.busy_sessions() == 0 && self.alive_terminals() == 0
    }

    async fn drain_title_tasks(&self) {
        loop {
            let batch: Vec<_> = std::mem::take(&mut *lock(&self.title_tasks));
            if batch.is_empty() {
                break;
            }
            for handle in batch {
                let _ = handle.await;
            }
        }
    }

    async fn shutdown_all(&self) {
        self.shutdown.cancel();
        let open: Vec<_> = lock(&self.sessions).keys().cloned().collect();
        for thread in open {
            self.close(&thread).await;
        }
        self.drain_title_tasks().await;
        #[cfg(feature = "workspace")]
        self.workspace.close();
    }

    fn notify(&self, method: &str, params: impl Serialize) {
        let line = json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        broadcast(&self.clients, &line);
    }

    fn receive(self: &Arc<Self>, client_id: u64, out: &ClientOut, line: &[u8]) {
        self.touch();
        match serde_json::from_slice::<JsonRpcMessage>(line) {
            Ok(JsonRpcMessage::Request(request)) => {
                let server = self.clone();
                let out = out.clone();
                let params = request.params.map_or(Value::Null, |params| json!(params));
                tokio::spawn(async move {
                    server
                        .dispatch(client_id, &out, request.id, &request.method, params)
                        .await;
                    server.touch();
                });
            }
            Ok(JsonRpcMessage::Notification(note)) if note.method == "$/cancelRequest" => {
                textgen::maybe_cancel_notification(self, client_id, note.params.map(|p| json!(p)));
            }
            Ok(_) => {}
            Err(error) => respond(out, Id::Null, Err(failure(-32700, error))),
        }
    }

    async fn dispatch(
        self: &Arc<Self>,
        client_id: u64,
        out: &ClientOut,
        id: Id,
        method: &str,
        params: Value,
    ) {
        if matches!(
            method,
            "session/open" | "session/fork" | "session/reload" | "session/snapshot"
        ) {
            match self.open_snapshot(method, params).await {
                Ok(live) => self.snapshot(out, id, &live),
                Err(error) => respond(out, id, Err(error)),
            }
            return;
        }
        let reply = match method {
            "initialize" => self.initialize(params),
            "daemon/status" => value(self.daemon_status()),
            "trust" => self.trust(params).await,
            _ if method.starts_with("session/") || turns::handles(method) => {
                self.dispatch_session(method, params).await
            }
            _ => self.dispatch_module(client_id, &id, method, params).await,
        };
        respond(out, id, reply);
    }

    async fn open_snapshot(
        self: &Arc<Self>,
        method: &str,
        params: Value,
    ) -> Result<Arc<Live>, rpc::Error> {
        match method {
            "session/open" => self.open(params).await,
            "session/fork" => self.fork(params).await,
            "session/reload" => {
                let p: ThreadParams = parse(params)?;
                self.reload(&p.thread_id).await
            }
            _ => parse::<ThreadParams>(params).and_then(|p| self.live(&p.thread_id)),
        }
    }

    async fn dispatch_session(self: &Arc<Self>, method: &str, params: Value) -> Reply {
        if turns::handles(method) {
            return self.turn(method, params).await;
        }
        match method {
            "session/list" => self.list(params).await,
            "session/submit" => {
                let p: SubmitParams = parse(params)?;
                self.submit(&p.thread_id, p.op).await.and_then(value)
            }
            "session/branch" => self.branch(params).await,
            "session/close" => {
                let p: ThreadParams = parse(params)?;
                self.close(&p.thread_id).await;
                Ok(Value::Null)
            }
            _ => self.dispatch_session_meta(method, params).await,
        }
    }

    async fn dispatch_session_meta(self: &Arc<Self>, method: &str, params: Value) -> Reply {
        match method {
            "session/rename" => self.rename(params).await,
            "session/archive" => self.archive(params).await,
            "session/trash" => self.trash(params).await,
            _ => Err(failure(-32601, format!("unknown method {method}"))),
        }
    }

    async fn dispatch_module(
        self: &Arc<Self>,
        client_id: u64,
        id: &Id,
        method: &str,
        params: Value,
    ) -> Reply {
        if providers::handles(method) {
            return self.dispatch_providers(method, params).await;
        }
        if mcp::handles(method) {
            return self.dispatch_mcp(method, params).await;
        }
        #[cfg(feature = "workspace")]
        if workspace::handles(method) {
            return workspace::dispatch(self, client_id, id, method, params).await;
        }
        let _ = (client_id, id);
        self.dispatch_extensions(method, params).await
    }

    async fn dispatch_mcp(self: &Arc<Self>, method: &str, params: Value) -> Reply {
        let notify = |authorize| self.notify("mcp/authorize", authorize);
        let reply = mcp::handle(&self.home, method, params, notify).await;
        if reply.is_ok() && mcp::edits(method) {
            self.refresh();
        }
        reply
    }

    async fn dispatch_extensions(self: &Arc<Self>, method: &str, params: Value) -> Reply {
        if agents::handles(method) {
            return agents::handle(&self.home, method, params).await;
        }
        let (reply, edited) = if hooks::handles(method) {
            (
                hooks::handle(&self.home, method, params).await,
                method != "hooks/list",
            )
        } else if prompts::handles(method) {
            (
                prompts::handle(&self.home, method, params).await,
                method != "prompts/list",
            )
        } else if skills::handles(method) {
            (
                skills::handle(&self.home, method, params).await,
                method == "skills/enable",
            )
        } else {
            return Err(failure(-32601, format!("unknown method {method}")));
        };
        if reply.is_ok() && edited {
            self.refresh();
        }
        reply
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
    fn snapshot(&self, out: &ClientOut, id: Id, live: &Live) {
        let projection = lock(&live.projection);
        let thread_id = match projection.snapshot().first() {
            Some(event) => event.thread_id.clone(),
            None => return respond(out, id, Err(internal("session has not started"))),
        };
        respond(
            out,
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
            stale: AtomicBool::new(false),
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
                Ok(event) => self.forward_event(&thread, &projection, &ready, event),
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
        self.touch();
    }

    fn forward_event(
        self: &Arc<Self>,
        thread: &ThreadId,
        projection: &Mutex<Projection>,
        ready: &watch::Sender<Option<SessionEpoch>>,
        event: Event,
    ) {
        let settled = matches!(
            event.event,
            EventKind::TurnCompleted { .. } | EventKind::CompactionEnded { .. }
        );
        let settled_live = settled.then(|| self.live(thread).ok()).flatten();
        {
            let mut projection = lock(projection);
            if let Some(texts) = projection.take_initial_title_trigger(&event)
                && let Some(live) = &settled_live
            {
                textgen::spawn_session_title(
                    self.clone(),
                    live.path.clone(),
                    texts,
                    self.shutdown.child_token(),
                );
            }
            let seq = projection.apply(&event);
            let epoch =
                matches!(event.event, EventKind::ThreadStarted { .. }).then_some(event.epoch);
            self.notify("session/event", SessionEvent { seq, event });
            if epoch.is_some() {
                ready.send_replace(epoch);
            }
        }
        self.touch();
        if let Some(live) = settled_live {
            self.settle(thread, &live);
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
    async fn reload(self: &Arc<Self>, thread: &ThreadId) -> Result<Arc<Live>, rpc::Error> {
        let guard = self.reloading.lock().await;
        let generation = self.config_gen.load(Ordering::SeqCst);
        let live = self.live(thread)?;
        let options = Options {
            mode: mode(&live),
            ..Default::default()
        };
        let epoch = SessionEpoch(lock(&live.projection).epoch.0 + 1);
        self.close(thread).await;
        let reloaded = self
            .start(
                options,
                Open::Path(live.path.clone()),
                epoch,
                Some(live.projection.clone()),
            )
            .await?
            .1;
        drop(guard);
        if self.config_gen.load(Ordering::SeqCst) != generation {
            reloaded.stale.store(true, Ordering::SeqCst);
            self.settle(thread, &reloaded);
        }
        Ok(reloaded)
    }

    async fn dispatch_providers(self: &Arc<Self>, method: &str, params: Value) -> Reply {
        let archiving = params["key"] == "sessions.archive_after_days";
        let runtime_config = method == "config/update" && runtime_config_key(&params);
        let reply = providers::handle(&self.home, method, params).await;
        if reply.is_ok() {
            if method == "providers/reload" {
                self.await_idle_refresh().await;
            } else if providers::edits(method) || runtime_config {
                self.refresh();
            }
            if archiving {
                self.sweep.notify_one();
            }
        }
        reply
    }

    fn mark_open_sessions_stale(&self) -> Vec<(ThreadId, Arc<Live>)> {
        self.config_gen.fetch_add(1, Ordering::SeqCst);
        let open: Vec<_> = lock(&self.sessions)
            .iter()
            .map(|(thread, live)| (thread.clone(), live.clone()))
            .collect();
        for (_, live) in &open {
            live.stale.store(true, Ordering::SeqCst);
        }
        open
    }

    /// Marks each open session stale so it reloads the changed configuration.
    fn refresh(self: &Arc<Self>) {
        for (thread, live) in self.mark_open_sessions_stale() {
            self.settle(&thread, &live);
        }
    }

    async fn await_idle_refresh(self: &Arc<Self>) {
        for (thread, live) in self.mark_open_sessions_stale() {
            if !lock(&live.projection).busy() && live.stale.swap(false, Ordering::SeqCst) {
                self.reload_or_close(thread).await;
            }
        }
    }

    async fn reload_or_close(self: &Arc<Self>, thread: ThreadId) {
        if let Err(error) = self.reload(&thread).await {
            self.notify(
                "session/closed",
                SessionClosed {
                    thread_id: thread,
                    error: Some(error.message),
                },
            );
        }
    }

    /// Reloads a stale session, unless that would cut off a turn, an approval, a compaction
    /// or queued input; it is tried again as each ends.
    fn settle(self: &Arc<Self>, thread: &ThreadId, live: &Live) {
        if lock(&live.projection).busy() || !live.stale.swap(false, Ordering::SeqCst) {
            return;
        }
        let server = self.clone();
        let thread = thread.clone();
        tokio::spawn(async move {
            server.reload_or_close(thread).await;
        });
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

    /// Archives sessions whose last message is older than `sessions.archive_after_days`, at start,
    /// daily and when the setting changes, notifying `session/archived` for each. Open sessions
    /// are left alone.
    async fn archive_inactive(self: Arc<Self>) {
        loop {
            let home = self.home.clone();
            let open: Vec<_> = lock(&self.sessions)
                .values()
                .map(|live| live.path.clone())
                .collect();
            match blocking(move |cancel| archive_before(&home, &open, cancel)).await {
                Ok(archived) => {
                    for path in archived {
                        self.notify(
                            "session/archived",
                            ArchiveParams {
                                path: path.display().to_string(),
                                archived: true,
                            },
                        );
                    }
                }
                Err(error) => eprintln!("chui app-server: archiving: {}", error.message),
            }
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(24 * 60 * 60)) => {}
                () = self.sweep.notified() => {}
            }
        }
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
        // Rewind storage follows the session, next to it in the trash.
        let moved = blocking(move |cancel| {
            let rewind = turns::storage_of(&home, &path, cancel);
            let moved = tiechui_session::trash(&home, &path).map_err(|e| e.to_string())?;
            if let Some(rewind) = rewind {
                std::fs::rename(&rewind, moved.with_extension("rewind"))
                    .map_err(|e| format!("{}: {e}", rewind.display()))?;
            }
            Ok(moved)
        })
        .await?;
        value(PathParams {
            path: moved.display().to_string(),
        })
    }

    async fn turn(&self, method: &str, params: Value) -> Reply {
        let params: TurnParams = parse(params)?;
        let path = self.live(&params.thread_id)?.path.clone();
        let home = self.home.clone();
        let method = method.to_owned();
        blocking(move |cancel| turns::handle(&home, &path, &method, &params.item_id, cancel)).await
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

fn runtime_config_key(params: &Value) -> bool {
    const KEYS: [&str; 6] = [
        "lightweight_model",
        "permissions.classifier_model",
        "network.proxy",
        "network.no_proxy",
        "network.ca_cert",
        "ask.auto_continue_after_sec",
    ];
    params["key"]
        .as_str()
        .is_some_and(|key| KEYS.contains(&key))
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

/// The sessions it archived.
fn archive_before(
    home: &Path,
    open: &[PathBuf],
    cancel: &CancellationToken,
) -> Result<Vec<PathBuf>, String> {
    let config = crate::setup::user_config(&home.join("config.toml"))?;
    let Some(days) = config.sessions.archive_after_days else {
        return Ok(vec![]);
    };
    let now = tiechui_session::now_ms();
    let before = now.saturating_sub(days.saturating_mul(24 * 60 * 60 * 1000));
    let (summaries, _) =
        tiechui_session::summaries(home, None, cancel).map_err(|e| e.to_string())?;
    let mut archived = vec![];
    for summary in summaries {
        if summary.meta.archived_at.is_none()
            && summary.modified < before
            && !open.contains(&summary.path)
        {
            tiechui_session::update_meta(&summary.path, |meta| meta.archived_at = Some(now))
                .map_err(|e| e.to_string())?;
            archived.push(summary.path);
        }
    }
    Ok(archived)
}

async fn update_meta(
    path: String,
    change: impl FnOnce(&mut tiechui_session::Meta) + Send + 'static,
) -> Reply {
    blocking(move |_| {
        tiechui_session::update_meta(Path::new(&path), change).map_err(|e| e.to_string())
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

#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub(super) mod tests {
    use super::*;
    use tiechui_protocol::{Turn, TurnId, TurnStatus};

    pub(in crate::app_server) struct TestClient {
        server: Arc<Server>,
        client_id: u64,
        out: ClientOut,
        lines: mpsc::UnboundedReceiver<String>,
        buffered: Vec<Value>,
        next_id: i64,
    }

    impl TestClient {
        pub(in crate::app_server) fn new(server: Arc<Server>) -> Self {
            let (client_id, out, lines) = server.add_client();
            Self {
                server,
                client_id,
                out,
                lines,
                buffered: Vec::new(),
                next_id: 0,
            }
        }

        pub(in crate::app_server) async fn call(
            &mut self,
            method: &str,
            params: Value,
        ) -> Result<Value, Value> {
            self.next_id += 1;
            let req_id = self.next_id;
            self.server
                .dispatch(
                    self.client_id,
                    &self.out,
                    Id::Number(req_id),
                    method,
                    params,
                )
                .await;
            self.wait_reply(req_id).await
        }

        pub(in crate::app_server) async fn call_ok<T: DeserializeOwned>(
            &mut self,
            method: &str,
            params: Value,
        ) -> T {
            let value = self
                .call(method, params)
                .await
                .unwrap_or_else(|error| panic!("unexpected rpc error on {method}: {error}"));
            serde_json::from_value(value).unwrap()
        }

        #[cfg(feature = "workspace")]
        pub(in crate::app_server) fn send_async(&mut self, method: &str, params: Value) -> i64 {
            self.next_id += 1;
            let req_id = self.next_id;
            let raw = json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "method": method,
                "params": params
            })
            .to_string();
            self.server
                .receive(self.client_id, &self.out, raw.as_bytes());
            req_id
        }

        #[cfg(feature = "workspace")]
        pub(in crate::app_server) fn send_notification(&self, method: &str, params: Value) {
            let raw = json!({
                "jsonrpc": "2.0",
                "method": method,
                "params": params
            })
            .to_string();
            self.server
                .receive(self.client_id, &self.out, raw.as_bytes());
        }

        pub(in crate::app_server) async fn wait_reply(
            &mut self,
            req_id: i64,
        ) -> Result<Value, Value> {
            loop {
                let raw = self.lines.recv().await.expect("missing rpc reply");
                let msg: Value = serde_json::from_str(&raw).unwrap();
                if msg.get("id") != Some(&json!(req_id)) {
                    self.buffered.push(msg);
                    continue;
                }
                return match msg.get("error") {
                    Some(error) => Err(error.clone()),
                    None => Ok(msg["result"].clone()),
                };
            }
        }

        pub(in crate::app_server) async fn wait_matching<T>(
            &mut self,
            label: &str,
            mut pred: impl FnMut(&Value) -> Option<T>,
        ) -> T {
            tokio::time::timeout(Duration::from_secs(10), async {
                let mut idx = 0;
                loop {
                    while idx < self.buffered.len() {
                        if let Some(found) = pred(&self.buffered[idx]) {
                            self.buffered.remove(idx);
                            return found;
                        }
                        idx += 1;
                    }
                    let raw = self.lines.recv().await.expect("expected notification");
                    self.buffered.push(serde_json::from_str(&raw).unwrap());
                }
            })
            .await
            .unwrap_or_else(|_| panic!("timed out waiting for {label}"))
        }

        pub(in crate::app_server) async fn next_event(
            &mut self,
            thread_id: &ThreadId,
        ) -> EventKind {
            self.wait_matching("session/event", |msg| {
                if msg.get("method") != Some(&json!("session/event")) {
                    return None;
                }
                let ev: SessionEvent = serde_json::from_value(msg["params"].clone()).unwrap();
                (&ev.event.thread_id == thread_id).then_some(ev.event.event)
            })
            .await
        }

        #[cfg(feature = "workspace")]
        pub(in crate::app_server) async fn wait_turn_completed(
            &mut self,
            thread_id: &ThreadId,
        ) -> Vec<EventKind> {
            let mut seen = Vec::new();
            loop {
                let kind = self.next_event(thread_id).await;
                let done = matches!(
                    &kind,
                    EventKind::TurnCompleted { turn } if turn.status == TurnStatus::Completed
                );
                seen.push(kind);
                if done {
                    return seen;
                }
            }
        }

        #[cfg(feature = "workspace")]
        pub(in crate::app_server) async fn wait_sessions_changed(&mut self) {
            self.wait_matching("sessions/changed", |msg| {
                (msg.get("method") == Some(&json!("sessions/changed"))).then_some(())
            })
            .await;
        }

        #[cfg(feature = "workspace")]
        pub(in crate::app_server) async fn wait_reloaded(&mut self, thread_id: &ThreadId) {
            self.wait_matching("session reload", |msg| {
                if msg.get("method") != Some(&json!("session/event")) {
                    return None;
                }
                let ev: SessionEvent = serde_json::from_value(msg["params"].clone()).unwrap();
                (&ev.event.thread_id == thread_id
                    && ev.event.epoch.0 >= 1
                    && matches!(ev.event.event, EventKind::ThreadStarted { .. }))
                .then_some(())
            })
            .await;
        }

        pub(in crate::app_server) fn drain_notices(&mut self) {
            self.buffered.clear();
            while self.lines.try_recv().is_ok() {}
        }

        #[cfg(feature = "workspace")]
        pub(in crate::app_server) async fn drain_title_tasks(&self) {
            self.server.drain_title_tasks().await;
        }

        pub(in crate::app_server) async fn close(self) {
            self.server.remove_client(self.client_id);
            self.server.shutdown_all().await;
        }
    }

    pub(in crate::app_server) fn sse_chat(content: &str) -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            json!({
                "choices": [{
                    "index": 0,
                    "delta": { "content": content },
                    "finish_reason": "stop"
                }]
            })
        )
    }

    pub(in crate::app_server) fn isolated_home_child(
        marker: &str,
        test_name: &str,
    ) -> Option<PathBuf> {
        if let Some(home) = std::env::var_os(marker) {
            return Some(PathBuf::from(home));
        }
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let mut cmd = std::process::Command::new(std::env::current_exe().unwrap());
        cmd.args(["--exact", test_name, "--nocapture"]);
        cmd.envs([
            (marker, home.as_os_str()),
            ("TIECHUI_HOME", home.as_os_str()),
            ("HOME", dir.path().as_os_str()),
        ]);
        let out = cmd.output().unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(
            out.status.success() && stdout.contains("1 passed"),
            "child {test_name} failed:\n{stdout}\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
        None
    }

    #[tokio::test]
    async fn daemon_status_counts_clients_busy_sessions_and_terminals() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::new(dir.path().join("home"));
        let mut client = TestClient::new(server.clone());

        let initial: DaemonStatus = client.call_ok("daemon/status", json!({})).await;
        assert_eq!(initial.pid, std::process::id());
        assert_eq!(initial.server_version, env!("CARGO_PKG_VERSION"));
        assert_eq!(initial.protocol_version, APP_PROTOCOL_VERSION);
        assert_eq!(initial.connected_clients, 1);
        assert_eq!(initial.busy_sessions, 0);
        assert_eq!(initial.alive_terminals, 0);

        let (c2, _out2, _lines2) = server.add_client();
        assert_eq!(
            client
                .call_ok::<DaemonStatus>("daemon/status", json!({}))
                .await
                .connected_clients,
            2
        );

        let projection = Arc::new(Mutex::new(Projection::default()));
        let (submissions, _rx) = mpsc::channel(1);
        let (_ready_tx, started) = watch::channel(Some(SessionEpoch(1)));
        let thread = ThreadId("t-unit".into());
        lock(&server.sessions).insert(
            thread.clone(),
            Arc::new(Live {
                path: dir.path().join("session.jsonl"),
                cwd: dir.path().to_path_buf(),
                submissions,
                shutdown: CancellationToken::new(),
                projection: projection.clone(),
                started,
                pump: Mutex::new(None),
                stale: AtomicBool::new(false),
            }),
        );
        assert_eq!(
            client
                .call_ok::<DaemonStatus>("daemon/status", json!({}))
                .await
                .busy_sessions,
            0
        );

        lock(&projection).apply(&Event {
            submission_id: None,
            thread_id: thread.clone(),
            epoch: SessionEpoch(1),
            event: EventKind::TurnStarted {
                turn: Turn {
                    id: TurnId("turn-1".into()),
                    status: TurnStatus::InProgress,
                    items: vec![],
                    started_at: 1,
                    ended_at: None,
                },
            },
        });
        assert_eq!(
            client
                .call_ok::<DaemonStatus>("daemon/status", json!({}))
                .await
                .busy_sessions,
            1
        );

        lock(&projection).apply(&Event {
            submission_id: None,
            thread_id: thread.clone(),
            epoch: SessionEpoch(1),
            event: EventKind::TurnCompleted {
                turn: Turn {
                    id: TurnId("turn-1".into()),
                    status: TurnStatus::Completed,
                    items: vec![],
                    started_at: 1,
                    ended_at: Some(2),
                },
            },
        });
        assert_eq!(
            client
                .call_ok::<DaemonStatus>("daemon/status", json!({}))
                .await
                .busy_sessions,
            0
        );

        #[cfg(feature = "workspace")]
        {
            server
                .workspace
                .create_terminal(tiechui_protocol::workspace::TerminalCreateParams {
                    id: "unit-term".into(),
                    cwd: dir.path().display().to_string(),
                    cols: 80,
                    rows: 24,
                    env: None,
                })
                .unwrap();
            assert_eq!(
                client
                    .call_ok::<DaemonStatus>("daemon/status", json!({}))
                    .await
                    .alive_terminals,
                1
            );
            server.workspace.kill_terminal("unit-term");
            assert_eq!(
                client
                    .call_ok::<DaemonStatus>("daemon/status", json!({}))
                    .await
                    .alive_terminals,
                0
            );
        }

        server.remove_client(c2);
        assert_eq!(
            client
                .call_ok::<DaemonStatus>("daemon/status", json!({}))
                .await
                .connected_clients,
            1
        );
        client.close().await;
    }

    #[tokio::test]
    async fn providers_presets_returns_matched_entries_without_marking_sessions_stale() {
        let dir = tempfile::tempdir().unwrap();
        let server = Server::new(dir.path().join("home"));
        let mut client = TestClient::new(server.clone());

        let (submissions, _rx) = mpsc::channel(1);
        let (_ready_tx, started) = watch::channel(Some(SessionEpoch(1)));
        let live = Arc::new(Live {
            path: dir.path().join("session.jsonl"),
            cwd: dir.path().to_path_buf(),
            submissions,
            shutdown: CancellationToken::new(),
            projection: Arc::new(Mutex::new(Projection::default())),
            started,
            pump: Mutex::new(None),
            stale: AtomicBool::new(false),
        });
        lock(&server.sessions).insert(ThreadId("t-presets".into()), live.clone());

        let matched: Vec<ModelEntry> = client
            .call_ok(
                "providers/presets",
                json!({
                    "ids": [
                        "gpt-5.4",
                        "unknown-model",
                        "anthropic/claude-sonnet-4.5",
                        "GLM-5.3-Flash",
                        "kimi-k2-0905-preview"
                    ]
                }),
            )
            .await;

        let returned_ids: Vec<&str> = matched.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            returned_ids,
            vec![
                "gpt-5.4",
                "anthropic/claude-sonnet-4.5",
                "GLM-5.3-Flash",
                "kimi-k2-0905-preview"
            ]
        );
        assert_eq!(matched[0].context_window, 1_050_000);
        assert!(matched[0].images);
        assert_eq!(matched[1].context_window, 200_000);
        assert!(matched[2].images);
        assert!(!matched[3].images);
        assert!(!live.stale.load(Ordering::Relaxed));

        let empty: Vec<ModelEntry> = client
            .call_ok("providers/presets", json!({ "ids": ["not-in-presets"] }))
            .await;
        assert!(empty.is_empty());

        client.close().await;
    }

    #[tokio::test]
    async fn providers_presets_round_trip_through_save_model_and_list() {
        let dir = tempfile::tempdir().unwrap();
        let mut client = TestClient::new(Server::new(dir.path().join("home")));

        let _: Value = client
            .call_ok(
                "providers/save",
                json!({
                    "name": "gateway",
                    "wire": "openai-completions",
                    "base_url": "http://127.0.0.1:9",
                    "api_key": "sk-test"
                }),
            )
            .await;

        let ids = vec![
            "claude-opus-5",
            "claude-sonnet-4-5",
            "gpt-5.4",
            "gpt-4o",
            "gemini-3-flash",
            "gemini-2.5-pro",
            "deepseek-v4-pro",
            "qwen3.8-max",
            "glm-5.3-flash",
            "kimi-k3",
        ];
        let presets: Vec<ModelEntry> = client
            .call_ok("providers/presets", json!({ "ids": ids }))
            .await;
        assert_eq!(presets.len(), 10);

        for model in &presets {
            let _: Value = client
                .call_ok(
                    "providers/save_model",
                    json!({
                        "provider": "gateway",
                        "previous": null,
                        "model": model
                    }),
                )
                .await;
        }

        let listed: Vec<ProviderEntry> = client.call_ok("providers/list", json!({})).await;
        assert_eq!(listed[0].models, presets);

        client.close().await;
    }

    #[tokio::test]
    async fn config_get_and_update_manage_lightweight_and_classifier_models() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path().join("home");
        let mut client = TestClient::new(Server::new(home.clone()));

        let _: Value = client
            .call_ok(
                "providers/save",
                json!({
                    "name": "fixture",
                    "wire": "openai-completions",
                    "base_url": "http://127.0.0.1:9",
                    "api_key": "sk-test"
                }),
            )
            .await;
        for id in ["main", "fast", "openrouter/judge"] {
            let _: Value = client
                .call_ok(
                    "providers/save_model",
                    json!({
                        "provider": "fixture",
                        "previous": null,
                        "model": {
                            "id": id,
                            "context_window": 200_000,
                            "max_output": 8_192,
                            "levels": ["off"],
                            "mapping": {},
                            "images": false
                        }
                    }),
                )
                .await;
        }
        let _: Value = client
            .call_ok(
                "config/update",
                json!({ "key": "model", "value": "fixture/main" }),
            )
            .await;

        let raw_get: Value = client.call_ok("config/get", json!({})).await;
        assert!(raw_get.get("lightweight_model").is_none());
        assert!(raw_get.get("classifier_model").is_none());
        let initial: ConfigValues = serde_json::from_value(raw_get).unwrap();
        assert_eq!(initial.lightweight_model, None);
        assert_eq!(initial.classifier_model, None);

        let _: Value = client
            .call_ok(
                "config/update",
                json!({ "key": "lightweight_model", "value": "fixture/fast" }),
            )
            .await;
        let _: Value = client
            .call_ok(
                "config/update",
                json!({
                    "key": "permissions.classifier_model",
                    "value": "fixture/openrouter/judge"
                }),
            )
            .await;

        let updated: ConfigValues = client.call_ok("config/get", json!({})).await;
        assert_eq!(updated.lightweight_model.as_deref(), Some("fixture/fast"));
        assert_eq!(
            updated.classifier_model.as_deref(),
            Some("fixture/openrouter/judge")
        );

        let config_path = home.join("config.toml");
        let saved_toml = std::fs::read_to_string(&config_path).unwrap();
        assert!(saved_toml.contains("lightweight_model = \"fixture/fast\""));
        assert!(saved_toml.contains("classifier_model = \"fixture/openrouter/judge\""));

        for key in ["lightweight_model", "permissions.classifier_model"] {
            for bad in [
                json!("fixture/missing"),
                json!("unknown/fast"),
                json!("no-provider"),
                json!("fixture/"),
                json!("/fast"),
                json!(42),
                json!(true),
                json!(["fixture/fast"]),
            ] {
                assert!(
                    client
                        .call("config/update", json!({ "key": key, "value": bad }))
                        .await
                        .is_err(),
                    "expected error for {key} = {bad}"
                );
            }
        }
        assert_eq!(std::fs::read_to_string(&config_path).unwrap(), saved_toml);

        for id in ["fast", "openrouter/judge"] {
            let _: Value = client
                .call_ok(
                    "providers/delete_model",
                    json!({ "provider": "fixture", "id": id }),
                )
                .await;
        }
        for key in ["lightweight_model", "permissions.classifier_model"] {
            let _: Value = client
                .call_ok("config/update", json!({ "key": key, "value": null }))
                .await;
        }

        let cleared_raw: Value = client.call_ok("config/get", json!({})).await;
        assert!(cleared_raw.get("lightweight_model").is_none());
        assert!(cleared_raw.get("classifier_model").is_none());
        let cleared: ConfigValues = serde_json::from_value(cleared_raw).unwrap();
        assert_eq!(cleared.lightweight_model, None);
        assert_eq!(cleared.classifier_model, None);

        let cleared_toml = std::fs::read_to_string(&config_path).unwrap();
        assert!(
            !cleared_toml.contains("lightweight_model"),
            "{cleared_toml}"
        );
        assert!(!cleared_toml.contains("classifier_model"), "{cleared_toml}");

        client.close().await;
    }

    #[tokio::test]
    async fn providers_reload_updates_open_session_models_without_reopening() {
        let Some(home) = isolated_home_child(
            "TIECHUI_TEST_PROVIDERS_RELOAD_HOME",
            "app_server::tests::providers_reload_updates_open_session_models_without_reopening",
        ) else {
            return;
        };
        run_providers_reload_case(home).await;
    }

    async fn run_providers_reload_case(home: PathBuf) {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let work = home.parent().unwrap().join("work");
        std::fs::create_dir_all(&work).unwrap();

        let mock = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_raw(sse_chat("synced-ok"), "text/event-stream"),
            )
            .expect(1)
            .mount(&mock)
            .await;

        std::fs::write(home.join("config.toml"), "model = \"alpha/m1\"\n").unwrap();
        let providers_path = home.join("providers.toml");
        std::fs::write(
            &providers_path,
            format!(
                "[alpha]\nwire = \"openai-completions\"\nbase_url = \"{}\"\napi_key = \"sk-alpha\"\nmodels = [{{ id = \"m1\", context_window = 128000, max_output = 4096 }}]\n",
                mock.uri()
            ),
        )
        .unwrap();

        let mut client = TestClient::new(Server::new(home));
        let opened: Snapshot = client.call_ok("session/open", json!({ "cwd": work })).await;
        let thread_id = opened.thread_id;
        client.drain_notices();

        std::fs::write(
            &providers_path,
            format!(
                "[alpha]\nwire = \"openai-completions\"\nbase_url = \"{uri}\"\napi_key = \"sk-alpha\"\nmodels = [\n  {{ id = \"m1\", context_window = 128000, max_output = 4096 }},\n  {{ id = \"m2\", context_window = 200000, max_output = 8192 }},\n]\n\n[beta]\nwire = \"openai-completions\"\nbase_url = \"{uri}\"\napi_key = \"sk-beta\"\nmodels = [{{ id = \"m3\", context_window = 256000, max_output = 16384 }}]\n",
                uri = mock.uri()
            ),
        )
        .unwrap();

        let reload_res: Value = client.call_ok("providers/reload", json!({})).await;
        assert_eq!(reload_res, Value::Null);

        let listed: Vec<ProviderEntry> = client.call_ok("providers/list", json!({})).await;
        assert_eq!(listed.len(), 2);

        let updated_models = wait_for_reloaded_models(&mut client, &thread_id).await;
        assert_eq!(updated_models, vec!["alpha/m1", "alpha/m2", "beta/m3"]);

        verify_post_reload_turn(&mut client, &thread_id).await;
        mock.verify().await;

        std::fs::write(&providers_path, "[broken\n").unwrap();
        assert!(client.call("providers/reload", json!({})).await.is_err());

        client.close().await;
    }

    async fn wait_for_reloaded_models(
        client: &mut TestClient,
        thread_id: &ThreadId,
    ) -> Vec<String> {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if let EventKind::ModelsUpdated { models } = client.next_event(thread_id).await {
                    let refs: Vec<String> = models
                        .iter()
                        .map(|m| format!("{}/{}", m.provider, m.id))
                        .collect();
                    if refs.contains(&"beta/m3".to_string()) {
                        return refs;
                    }
                }
            }
        })
        .await
        .expect("timed out waiting for models_updated")
    }

    async fn verify_post_reload_turn(client: &mut TestClient, thread_id: &ThreadId) {
        let _: Value = client
            .call_ok(
                "session/submit",
                json!({
                    "thread_id": thread_id,
                    "op": { "kind": "set_model", "provider": "beta", "model": "beta/m3" }
                }),
            )
            .await;
        let _: Value = client
            .call_ok(
                "session/submit",
                json!({
                    "thread_id": thread_id,
                    "op": {
                        "kind": "user_input",
                        "delivery": "steer",
                        "content": [{ "kind": "text", "text": "ping" }]
                    }
                }),
            )
            .await;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let EventKind::TurnCompleted { turn } = client.next_event(thread_id).await {
                    assert_eq!(turn.status, TurnStatus::Completed);
                    break;
                }
            }
        })
        .await
        .expect("turn did not complete on newly reloaded provider");
    }
}
