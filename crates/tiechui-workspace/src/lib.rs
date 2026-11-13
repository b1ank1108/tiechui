//! Files, Git and terminals under project roots for `chui app-server` clients. Independent
//! of the agent: each call names its root, and notifications go to a [`Sink`].

mod fs;
mod git;
mod ports;
mod search;
mod terminal;
mod watch;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard};

pub use fs::{create, delete, list, mkdir, read, rename, tree, write};
pub use git::{
    branches, branches_with_env, compare, compare_with_env, log, log_with_env, pull_request,
    pull_request_with_env, run, run_with_env, show, show_with_env, status, status_with_env,
};
pub use search::search;
use tiechui_protocol::workspace::{
    PortEntry, PortStopParams, TerminalAttached, TerminalCreateParams, TerminalResizeParams,
};

/// Receives a notification's method and params.
pub type Sink = Arc<dyn Fn(&'static str, serde_json::Value) + Send + Sync>;

#[derive(Debug)]
pub struct Error(pub String);

impl<E: std::fmt::Display> From<E> for Error {
    fn from(error: E) -> Self {
        Self(error.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

fn fail<T>(message: impl Into<String>) -> Result<T> {
    Err(Error(message.into()))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// An absolute project root.
pub fn root(root: &str) -> Result<PathBuf> {
    let path = PathBuf::from(root);
    if !path.is_absolute() {
        return fail(format!("{root} is not an absolute path"));
    }
    Ok(path)
}

pub struct Workspace {
    sink: Sink,
    watches: Mutex<HashMap<PathBuf, watch::Watch>>,
    terminals: Mutex<HashMap<String, Arc<terminal::Terminal>>>,
    ports: Arc<ports::PortMonitor>,
}

impl Workspace {
    pub fn new(sink: Sink) -> Self {
        let ports = ports::PortMonitor::new(sink.clone());
        Self {
            sink,
            watches: Mutex::default(),
            terminals: Mutex::default(),
            ports,
        }
    }

    /// Sends `fs/changed` and `git/changed` for the root until [`Self::unwatch`].
    pub fn watch(&self, root: &Path) -> Result<()> {
        let mut watches = lock(&self.watches);
        if !watches.contains_key(root) {
            watches.insert(
                root.to_owned(),
                watch::Watch::start(root, self.sink.clone())?,
            );
        }
        Ok(())
    }

    pub fn unwatch(&self, root: &Path) {
        lock(&self.watches).remove(root);
    }

    pub fn create_terminal(&self, params: TerminalCreateParams) -> Result<()> {
        let mut terminals = lock(&self.terminals);
        if terminals.contains_key(&params.id) {
            return fail(format!("terminal {} exists", params.id));
        }
        let terminal = terminal::Terminal::spawn(&params, self.sink.clone())?;
        terminals.insert(params.id, terminal);
        Ok(())
    }

    fn terminal(&self, id: &str) -> Result<Arc<terminal::Terminal>> {
        lock(&self.terminals)
            .get(id)
            .cloned()
            .ok_or_else(|| Error(format!("terminal {id} does not exist")))
    }

    pub fn attach_terminal(&self, id: &str) -> Result<TerminalAttached> {
        Ok(self.terminal(id)?.attached())
    }

    pub fn write_terminal(&self, id: &str, data: &str) -> Result<()> {
        self.terminal(id)?.write(data.as_bytes())
    }

    pub fn resize_terminal(&self, params: &TerminalResizeParams) -> Result<()> {
        self.terminal(&params.id)?.resize(params.cols, params.rows)
    }

    pub fn kill_terminal(&self, id: &str) {
        if let Some(terminal) = lock(&self.terminals).remove(id) {
            terminal.kill();
        }
    }

    pub fn alive_terminals(&self) -> usize {
        lock(&self.terminals)
            .values()
            .filter(|terminal| terminal.is_alive())
            .count()
    }

    pub fn list_ports(&self, roots: &[String]) -> Result<Vec<PortEntry>> {
        self.list_ports_for_client(0, roots)
    }

    pub fn list_ports_for_client(
        &self,
        client_id: u64,
        roots: &[String],
    ) -> Result<Vec<PortEntry>> {
        self.ports.list(client_id, roots)
    }

    pub fn remove_client(&self, client_id: u64) {
        self.ports.remove_client(client_id);
    }

    pub fn stop_port(&self, params: &PortStopParams) -> Result<()> {
        self.ports.stop(params)
    }

    /// Ends every terminal and watcher.
    pub fn close(&self) {
        self.ports.close();
        lock(&self.watches).clear();
        for (_, terminal) in lock(&self.terminals).drain() {
            terminal.kill();
        }
    }
}
