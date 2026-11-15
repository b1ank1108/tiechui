use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::Value;
#[cfg(feature = "workspace")]
use tiechui_core::textgen::TextGenError;
use tiechui_model::ModelId;
use tiechui_protocol::rpc::Id;
#[cfg(feature = "workspace")]
use tiechui_protocol::workspace::{
    CancelCommitMessageParams, CommitMessageParams, CommitMessageResult,
};
use tiechui_protocol::{Event, EventKind, InputContent, Item, KnownItem, TurnStatus};
use tiechui_providers::ModelProvider;
use tiechui_session::SessionStore;
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

#[cfg(feature = "workspace")]
use super::{Reply, failure, internal, parse, rpc, value};
use super::{Server, lock};

const MAX_PRE_CANCELLED: usize = 64;
#[cfg(feature = "workspace")]
const REQUEST_CANCELLED_CODE: i64 = -32800;

#[derive(Default)]
pub(super) struct CancelRegistry {
    by_rpc: HashMap<(u64, String), CancellationToken>,
    by_request_id: HashMap<String, (u64, CancellationToken)>,
    pre_cancelled_rpc: VecDeque<(u64, String)>,
    #[cfg(feature = "workspace")]
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

#[cfg(feature = "workspace")]
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
    #[cfg(feature = "workspace")]
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

    #[cfg(feature = "workspace")]
    pub(super) fn finish(&mut self, client_id: u64, rpc_id: &Id, request_id: Option<&str>) {
        self.by_rpc.remove(&(client_id, rpc_key(rpc_id)));
        if let Some(req_id) = request_id {
            self.by_request_id.remove(req_id);
        }
    }

    #[cfg(feature = "workspace")]
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

#[derive(Default)]
pub(crate) struct InitialTitleTrigger {
    eligible: Option<bool>,
}

impl InitialTitleTrigger {
    pub(crate) fn take(&mut self, event: &Event) -> Option<(String, String)> {
        match &event.event {
            EventKind::ThreadStarted { thread, replay } if self.eligible.is_none() => {
                self.eligible = Some(!*replay && thread.turns.is_empty());
                None
            }
            EventKind::TurnCompleted { turn }
                if turn.status == TurnStatus::Completed && self.eligible == Some(true) =>
            {
                self.eligible = Some(false);
                turn_title_texts(&turn.items)
            }
            _ => None,
        }
    }

    pub(crate) fn is_exhausted(&self) -> bool {
        self.eligible == Some(false)
    }
}

fn first_user_text(items: &[Item]) -> Option<String> {
    items.iter().find_map(|item| {
        let Item::Known(KnownItem::UserMessage { content, .. }) = item else {
            return None;
        };
        let parts: Vec<&str> = content
            .iter()
            .filter_map(|block| match block {
                InputContent::Text { text } if !text.is_empty() => Some(text.as_str()),
                _ => None,
            })
            .collect();
        (!parts.is_empty()).then(|| parts.join("\n"))
    })
}

fn turn_title_texts(items: &[Item]) -> Option<(String, String)> {
    let user_text = first_user_text(items)?;
    if user_text.trim().is_empty() {
        return None;
    }
    let assistant_parts: Vec<&str> = items
        .iter()
        .filter_map(|item| match item {
            Item::Known(KnownItem::AssistantMessage { text, .. }) if !text.is_empty() => {
                Some(text.as_str())
            }
            _ => None,
        })
        .collect();
    Some((user_text, assistant_parts.join("\n\n")))
}

struct TitleJob {
    provider: Box<dyn ModelProvider>,
    model: ModelId,
}

fn prepare_title_job(home: &Path, path: &Path, cancel: &CancellationToken) -> Option<TitleJob> {
    if !path.exists() || tiechui_session::meta(path).ok()?.title.is_some() {
        return None;
    }
    if SessionStore::open_read_only(path, cancel)
        .ok()?
        .header()
        .parent_session
        .is_some()
    {
        return None;
    }
    let (provider, model, _) = resolve_lightweight(home).ok()?;
    Some(TitleJob { provider, model })
}

pub(super) async fn generate_and_save_session_title(
    home: &Path,
    path: &Path,
    user_text: &str,
    assistant_text: &str,
    cancel: CancellationToken,
) -> bool {
    let job_home = home.to_owned();
    let job_path = path.to_owned();
    let job_cancel = cancel.clone();
    let Ok(Some(job)) =
        tokio::task::spawn_blocking(move || prepare_title_job(&job_home, &job_path, &job_cancel))
            .await
    else {
        return false;
    };
    let Some(title) = tiechui_core::textgen::generate_session_title(
        job.provider.as_ref(),
        job.model,
        user_text,
        assistant_text,
        cancel,
    )
    .await
    else {
        return false;
    };
    let save_path = path.to_owned();
    let saved =
        tokio::task::spawn_blocking(move || tiechui_session::set_initial_title(&save_path, &title))
            .await;
    matches!(saved, Ok(Ok(true)))
}

pub(crate) fn watch_session_title(
    home: PathBuf,
    path: PathBuf,
    mut events: broadcast::Receiver<Event>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut trigger = InitialTitleTrigger::default();
        loop {
            match events.recv().await {
                Ok(event) => {
                    if let Some((user_text, assistant_text)) = trigger.take(&event) {
                        generate_and_save_session_title(
                            &home,
                            &path,
                            &user_text,
                            &assistant_text,
                            CancellationToken::new(),
                        )
                        .await;
                        break;
                    }
                    if trigger.is_exhausted() {
                        break;
                    }
                }
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => break,
            }
        }
    })
}

pub(super) fn spawn_session_title(
    server: Arc<Server>,
    path: PathBuf,
    texts: (String, String),
    cancel: CancellationToken,
) {
    let (user_text, assistant_text) = texts;
    let task_server = server.clone();
    let handle = tokio::spawn(async move {
        if generate_and_save_session_title(
            &task_server.home,
            &path,
            &user_text,
            &assistant_text,
            cancel,
        )
        .await
        {
            task_server.notify("sessions/changed", Value::Null);
            task_server.touch();
        }
    });
    let mut tasks = lock(&server.title_tasks);
    tasks.retain(|task| !task.is_finished());
    tasks.push(handle);
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
