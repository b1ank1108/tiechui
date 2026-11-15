use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
use tiechui_core::textgen::TextGenError;
use tiechui_model::{Content, ModelId, Role};
use tiechui_protocol::rpc::{self, Id};
#[cfg(feature = "workspace")]
use tiechui_protocol::workspace::{
    CancelCommitMessageParams, CommitMessageParams, CommitMessageResult,
};
use tiechui_providers::ModelProvider;
use tiechui_session::{EntryKind, SessionStore};
use tokio_util::sync::CancellationToken;

#[cfg(feature = "workspace")]
use super::{Reply, internal, parse, value};
use super::{Server, failure, lock};

const MAX_PRE_CANCELLED: usize = 64;
const REQUEST_CANCELLED_CODE: i64 = -32800;

#[derive(Default)]
pub(super) struct CancelRegistry {
    by_rpc: HashMap<(u64, String), CancellationToken>,
    by_request_id: HashMap<String, (u64, CancellationToken)>,
    pre_cancelled_rpc: VecDeque<(u64, String)>,
    pre_cancelled: VecDeque<String>,
}

fn rpc_key(id: &Id) -> String {
    match id {
        Id::String(s) => format!("s:{s}"),
        Id::Number(n) => format!("n:{n}"),
        Id::Null => "null".into(),
    }
}

fn push_bounded<T: PartialEq>(queue: &mut VecDeque<T>, item: T) {
    if !queue.contains(&item) {
        if queue.len() >= MAX_PRE_CANCELLED {
            queue.pop_front();
        }
        queue.push_back(item);
    }
}

fn take_queued<T: PartialEq>(queue: &mut VecDeque<T>, item: &T) -> bool {
    match queue.iter().position(|queued| queued == item) {
        Some(pos) => {
            queue.remove(pos);
            true
        }
        None => false,
    }
}

impl CancelRegistry {
    pub(super) fn register(
        &mut self,
        client_id: u64,
        rpc_id: &Id,
        request_id: Option<&str>,
        parent: &CancellationToken,
    ) -> CancellationToken {
        let token = parent.child_token();
        let key = (client_id, rpc_key(rpc_id));
        let pre_rpc = take_queued(&mut self.pre_cancelled_rpc, &key);
        let pre_req =
            request_id.is_some_and(|id| take_queued(&mut self.pre_cancelled, &id.to_owned()));
        if pre_rpc || pre_req {
            token.cancel();
            return token;
        }
        self.by_rpc.insert(key, token.clone());
        if let Some(req_id) = request_id {
            self.by_request_id
                .insert(req_id.to_owned(), (client_id, token.clone()));
        }
        token
    }

    pub(super) fn finish(&mut self, client_id: u64, rpc_id: &Id, request_id: Option<&str>) {
        self.by_rpc.remove(&(client_id, rpc_key(rpc_id)));
        if let Some(req_id) = request_id {
            self.by_request_id.remove(req_id);
        }
    }

    pub(super) fn cancel_request_id(&mut self, request_id: &str) {
        if let Some((_, token)) = self.by_request_id.remove(request_id) {
            token.cancel();
        } else {
            push_bounded(&mut self.pre_cancelled, request_id.to_owned());
        }
    }

    pub(super) fn cancel_rpc(&mut self, client_id: u64, rpc_id: &Id) {
        let key = (client_id, rpc_key(rpc_id));
        if let Some(token) = self.by_rpc.remove(&key) {
            token.cancel();
        } else {
            push_bounded(&mut self.pre_cancelled_rpc, key);
        }
    }

    pub(super) fn remove_client(&mut self, client_id: u64) {
        self.pre_cancelled_rpc
            .retain(|(owner, _)| *owner != client_id);
        self.by_rpc.retain(|(owner, _), token| {
            if *owner == client_id {
                token.cancel();
                false
            } else {
                true
            }
        });
        self.by_request_id.retain(|_, (owner, token)| {
            if *owner == client_id {
                token.cancel();
                false
            } else {
                true
            }
        });
    }
}

fn resolve_lightweight(
    home: &Path,
) -> Result<
    (
        Box<dyn ModelProvider>,
        ModelId,
        tiechui_config::NetworkConfig,
    ),
    String,
> {
    let config = crate::setup::user_config(&home.join("config.toml"))?;
    let Some(raw) = config.lightweight_model.as_deref() else {
        return Err("lightweight_model is not configured".into());
    };
    let (provider_name, model_id) = raw
        .split_once('/')
        .filter(|(p, m)| !p.is_empty() && !m.is_empty())
        .ok_or_else(|| format!("invalid lightweight_model '{raw}'"))?;
    let providers =
        tiechui_config::load_providers(&home.join("providers.toml")).map_err(|e| e.to_string())?;
    let provider_cfg = providers
        .get(provider_name)
        .ok_or_else(|| format!("lightweight_model '{raw}' references unknown provider"))?;
    if !provider_cfg.models.iter().any(|m| m.id == model_id) {
        return Err(format!(
            "lightweight_model '{raw}' is not declared in providers.toml"
        ));
    }
    let provider = tiechui_providers::from_config(provider_name, provider_cfg, &config.network)
        .map_err(|e| e.to_string())?;
    Ok((provider, ModelId(raw.to_owned()), config.network))
}

struct TitleJob {
    provider: Box<dyn ModelProvider>,
    model: ModelId,
    user_text: String,
    assistant_text: String,
}

fn prepare_title_job(home: &Path, path: &Path, cancel: &CancellationToken) -> Option<TitleJob> {
    if !path.exists() || tiechui_session::meta(path).ok()?.title.is_some() {
        return None;
    }
    let (provider, model, _) = resolve_lightweight(home).ok()?;
    let store = SessionStore::open_read_only(path, cancel).ok()?;
    let (user_text, assistant_text) = first_turn_texts(&store)?;
    Some(TitleJob {
        provider,
        model,
        user_text,
        assistant_text,
    })
}

#[derive(Default)]
struct FirstTurnCollector {
    user_turns: usize,
    user_parts: Vec<String>,
    assistant_parts: Vec<String>,
}

impl FirstTurnCollector {
    fn push(&mut self, entry: &tiechui_session::Entry) {
        let EntryKind::Message { message, .. } = &entry.kind else {
            return;
        };
        match message.role {
            Role::User => {
                self.user_turns += 1;
                collect_message_text(&message.content, &mut self.user_parts);
            }
            Role::Assistant if self.user_turns == 1 => {
                collect_message_text(&message.content, &mut self.assistant_parts);
            }
            _ => {}
        }
    }

    fn finish(self) -> Option<(String, String)> {
        if self.user_turns != 1 {
            return None;
        }
        let user_text = self.user_parts.join("\n");
        (!user_text.trim().is_empty()).then(|| (user_text, self.assistant_parts.join("\n\n")))
    }
}

fn first_turn_texts(store: &SessionStore) -> Option<(String, String)> {
    if store.header().parent_session.is_some() {
        return None;
    }
    let branch = store.path_to_root(store.current_leaf()?).ok()?;
    let mut collector = FirstTurnCollector::default();
    for entry in branch {
        collector.push(entry);
    }
    collector.finish()
}

fn collect_message_text(content: &[Content], out: &mut Vec<String>) {
    for block in content {
        if let Content::Text { text, .. } = block
            && !text.is_empty()
        {
            out.push(text.clone());
        }
    }
}

pub(super) fn spawn_session_title(server: Arc<Server>, path: PathBuf, cancel: CancellationToken) {
    let task_server = server.clone();
    let handle = tokio::spawn(async move {
        run_session_title(task_server, path, cancel).await;
    });
    let mut tasks = lock(&server.title_tasks);
    tasks.retain(|task| !task.is_finished());
    tasks.push(handle);
}

async fn run_session_title(server: Arc<Server>, path: PathBuf, cancel: CancellationToken) {
    let home = server.home.clone();
    let job_path = path.clone();
    let job_cancel = cancel.clone();
    let Ok(Some(job)) =
        tokio::task::spawn_blocking(move || prepare_title_job(&home, &job_path, &job_cancel)).await
    else {
        return;
    };
    let Some(title) = tiechui_core::textgen::generate_session_title(
        job.provider.as_ref(),
        job.model,
        &job.user_text,
        &job.assistant_text,
        cancel,
    )
    .await
    else {
        return;
    };
    let saved =
        tokio::task::spawn_blocking(move || tiechui_session::set_initial_title(&path, &title))
            .await;
    if matches!(saved, Ok(Ok(true))) {
        server.notify("sessions/changed", Value::Null);
        server.touch();
    }
}

pub(super) fn maybe_cancel_notification(server: &Server, client_id: u64, params: Option<Value>) {
    let Some(id_val) = params.and_then(|mut p| p.get_mut("id").map(Value::take)) else {
        return;
    };
    if let Ok(id) = serde_json::from_value::<Id>(id_val) {
        lock(&server.cancels).cancel_rpc(client_id, &id);
    }
}

#[cfg(feature = "workspace")]
pub(super) fn handles_git(method: &str) -> bool {
    matches!(method, "git/commit_message" | "git/cancel_commit_message")
}

#[cfg(feature = "workspace")]
pub(super) async fn dispatch_git(
    server: &Server,
    client_id: u64,
    rpc_id: &Id,
    method: &str,
    params: Value,
) -> Reply {
    match method {
        "git/cancel_commit_message" => {
            let params: CancelCommitMessageParams = parse(params)?;
            lock(&server.cancels).cancel_request_id(&params.request_id);
            Ok(Value::Null)
        }
        "git/commit_message" => commit_message(server, client_id, rpc_id, params).await,
        _ => Err(failure(-32601, format!("unknown method {method}"))),
    }
}

#[cfg(feature = "workspace")]
async fn commit_message(server: &Server, client_id: u64, rpc_id: &Id, params: Value) -> Reply {
    let params: CommitMessageParams = parse(params)?;
    let req_id = params.request_id.as_deref();
    let cancel = lock(&server.cancels).register(client_id, rpc_id, req_id, &server.shutdown);
    let result = generate_commit_for_root(&server.home, &params.root, cancel).await;
    lock(&server.cancels).finish(client_id, rpc_id, req_id);
    result
}

#[cfg(feature = "workspace")]
async fn generate_commit_for_root(home: &Path, raw_root: &str, cancel: CancellationToken) -> Reply {
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    let home = home.to_owned();
    let raw_root = raw_root.to_owned();
    let prepared = tokio::task::spawn_blocking(move || prepare_commit_job(&home, &raw_root))
        .await
        .map_err(internal)??;
    if cancel.is_cancelled() {
        return Err(cancelled_error());
    }
    let (provider, model, diff) = prepared;
    match tiechui_core::textgen::generate_commit_message(provider.as_ref(), model, &diff, cancel)
        .await
    {
        Ok(message) => value(CommitMessageResult { message }),
        Err(TextGenError::Cancelled) => Err(cancelled_error()),
        Err(error) => Err(internal(error)),
    }
}

#[cfg(feature = "workspace")]
fn prepare_commit_job(
    home: &Path,
    raw_root: &str,
) -> Result<(Box<dyn ModelProvider>, ModelId, String), rpc::Error> {
    let (provider, model, network) = resolve_lightweight(home).map_err(internal)?;
    let root = tiechui_workspace::root(raw_root).map_err(|e| internal(e.0))?;
    let diff = tiechui_workspace::commit_diff_with_env(&root, &network.subprocess_env())
        .map_err(|e| internal(e.0))?;
    Ok((provider, model, diff))
}

#[cfg(feature = "workspace")]
fn cancelled_error() -> rpc::Error {
    failure(REQUEST_CANCELLED_CODE, "request cancelled")
}
