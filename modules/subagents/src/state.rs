use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};
use tiechui_plugin_api::{
    protocol::*,
    serde_json::{self, Value, json},
    *,
};
use tokio::sync::watch;

use crate::{output, params::PreparedTask};

pub(crate) const ENTRY: &str = "tiechui.subagents.record";
pub(crate) const CONTINUATION: &str = "tiechui.subagents.completed";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopCause {
    Turn,
    Request,
    Session,
}

pub(crate) struct ActiveTask {
    pub dispatch_id: String,
    pub cancel: CancellationToken,
    pub steer: tokio::sync::mpsc::UnboundedSender<String>,
    pub done: watch::Receiver<bool>,
    pub stop: Option<StopCause>,
    pub notified: bool,
}

pub(crate) struct ActiveDispatch {
    pub mode: String,
    pub cancel: CancellationToken,
    pub done: watch::Receiver<bool>,
    pub detached: bool,
    pub stop: Option<StopCause>,
    pub notified: bool,
}

#[derive(Clone)]
pub(crate) struct Notification {
    pub key: String,
    pub task_id: String,
    pub dispatch_id: String,
    pub xml: String,
}

#[derive(Clone)]
pub(crate) enum Target {
    Task {
        dispatch_id: String,
        index: usize,
        task_id: String,
    },
    Dispatch(String),
}

#[derive(Default)]
pub(crate) struct State {
    pub session: String,
    pub session_gen: u64,
    pub next_agent_id: u64,
    pub revision: u64,
    pub emitted: Option<u64>,
    pub notification_seq: u64,
    pub records: BTreeMap<String, AgentDispatch>,
    pub order: Vec<String>,
    pub dispatches: BTreeMap<String, ActiveDispatch>,
    pub tasks: BTreeMap<String, ActiveTask>,
    pub notifications: Vec<Notification>,
    pub pending_entries: Vec<(String, Value)>,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn parse_agent_seq(id: &str) -> u64 {
    id.strip_prefix("ag")
        .and_then(|raw| raw.parse::<u64>().ok())
        .unwrap_or(0)
}

fn cancel_dangling_tasks(record: &mut AgentDispatch, max_seq: &mut u64) {
    for task in &mut record.tasks {
        *max_seq = (*max_seq).max(parse_agent_seq(&task.id));
        if task.state.active() {
            task.state = AgentState::Cancelled;
            task.diagnostic = Some("Interrupted before record settlement".into());
        }
    }
}

pub(crate) fn restore_records(state: &mut State, session_id: &str, entries: &[BranchEntry]) {
    let mut max_seq = 0;
    for (_, value) in entries
        .iter()
        .filter_map(|e| e.custom.as_ref())
        .filter(|(kind, _)| kind == ENTRY)
    {
        let Ok(mut record) = serde_json::from_value::<AgentDispatch>(value.clone()) else {
            continue;
        };
        record.session_id = session_id.to_owned();
        cancel_dangling_tasks(&mut record, &mut max_seq);
        if !state.records.contains_key(&record.id) {
            state.order.push(record.id.clone());
        }
        state.records.insert(record.id.clone(), record);
    }
    state.next_agent_id = max_seq;
}

impl State {
    pub fn ordered_records(&self) -> impl DoubleEndedIterator<Item = &AgentDispatch> {
        let ordered = self.order.iter().filter_map(|id| self.records.get(id));
        let unindexed = self
            .records
            .iter()
            .filter(|(id, _)| !self.order.contains(id))
            .map(|(_, r)| r);
        ordered.chain(unindexed).collect::<Vec<_>>().into_iter()
    }

    pub fn view(&self, open: bool) -> SubagentsView {
        SubagentsView {
            records: self.ordered_records().cloned().collect(),
            open,
            pending_notifications: self.notifications.len(),
        }
    }

    pub fn restore(&mut self, session_id: &str, entries: &[BranchEntry]) {
        let _ = self.stop_all(StopCause::Session);
        self.dispatches.clear();
        self.tasks.clear();
        self.records.clear();
        self.order.clear();
        self.notifications.clear();
        self.pending_entries.clear();
        self.session = session_id.to_owned();
        self.session_gen = self.session_gen.wrapping_add(1);
        self.next_agent_id = 0;
        self.emitted = None;
        self.revision = self.revision.wrapping_add(1);
        restore_records(self, session_id, entries);
    }

    pub fn ensure_available_names(&self, incoming: &[PreparedTask]) -> Result<(), ToolError> {
        for candidate in incoming.iter().filter_map(|t| t.name.as_deref()) {
            for record in self.records.values() {
                if let Some(existing) = record
                    .tasks
                    .iter()
                    .find(|t| t.state.active() && t.name.as_deref() == Some(candidate))
                {
                    return Err(ToolError(format!(
                        "Subagent name '{candidate}' is already in use by active subagent {}",
                        existing.id
                    )));
                }
            }
        }
        Ok(())
    }

    pub fn allocate_id(&mut self) -> String {
        self.next_agent_id += 1;
        format!("ag{}", self.next_agent_id)
    }

    pub fn push_record_entry(&mut self, id: &str) {
        if let Some(record) = self.records.get(id).cloned() {
            self.pending_entries.push((ENTRY.into(), json!(record)));
        }
    }

    fn find_task_by(&self, predicate: impl Fn(&AgentTask) -> bool) -> Option<Target> {
        for record in self.ordered_records().rev() {
            for (index, task) in record.tasks.iter().enumerate().rev() {
                if predicate(task) {
                    return Some(Target::Task {
                        dispatch_id: record.id.clone(),
                        index,
                        task_id: task.id.clone(),
                    });
                }
            }
        }
        None
    }

    pub fn resolve_target(&self, raw: &str) -> Result<Target, ToolError> {
        let id = raw.trim();
        if id.is_empty() {
            return Err(ToolError("Subagent identifier must not be empty".into()));
        }
        if let Some(target) = self.find_task_by(|task| task.id == id) {
            return Ok(target);
        }
        if let Some(target) = self.find_task_by(|task| task.name.as_deref() == Some(id)) {
            return Ok(target);
        }
        if self.records.contains_key(id) {
            return Ok(Target::Dispatch(id.to_owned()));
        }
        Err(ToolError(format!(
            "No subagent or dispatch '{id}' in this session"
        )))
    }

    pub fn resolve_send_task(&self, raw: &str) -> Result<AgentTask, ToolError> {
        let id = raw.trim();
        if let Some(Target::Task {
            dispatch_id, index, ..
        }) = self
            .find_task_by(|task| task.id == id)
            .or_else(|| self.find_task_by(|task| task.name.as_deref() == Some(id)))
        {
            let task = &self.records[&dispatch_id].tasks[index];
            if task.state == AgentState::Skipped {
                return Err(ToolError(format!(
                    "Subagent {} was skipped and has no session to resume",
                    task.id
                )));
            }
            return Ok(task.clone());
        }
        if self.records.contains_key(id) {
            return Err(ToolError(format!(
                "'{id}' is a dispatch ID; pass an agent_id (e.g. ag1) or subagent name to subagent_send"
            )));
        }
        Err(ToolError(format!("No subagent '{id}' in this session")))
    }

    pub fn name_resolves_to(&self, name: &str, task_id: &str) -> bool {
        matches!(
            self.find_task_by(|t| t.name.as_deref() == Some(name)),
            Some(Target::Task { task_id: found, .. }) if found == task_id
        )
    }

    fn promote_dispatch_record(record: &mut AgentDispatch) -> bool {
        let mut changed = false;
        for task in &mut record.tasks {
            if task.state.active() && !task.background {
                task.background = true;
                changed = true;
            }
        }
        if !record.background && record.tasks.iter().any(|t| t.state.active()) {
            record.background = true;
            changed = true;
        }
        changed
    }

    fn promote_task_in_record(
        record: &mut AgentDispatch,
        index: usize,
        label: &str,
    ) -> Result<bool, ToolError> {
        if record.mode != "parallel" {
            if !record.tasks.iter().any(|t| t.state.active()) {
                return Err(ToolError(format!(
                    "{label} is already settled; only an active subagent can move to the background"
                )));
            }
            return Ok(Self::promote_dispatch_record(record));
        }
        let task = &mut record.tasks[index];
        if !task.state.active() {
            return Err(ToolError(format!(
                "{label} is {:?}; only an active subagent can move to the background",
                task.state
            )));
        }
        let mut changed = false;
        if !task.background {
            task.background = true;
            changed = true;
        }
        if !record
            .tasks
            .iter()
            .any(|t| !t.background && t.state.active())
            && !record.background
        {
            record.background = true;
            changed = true;
        }
        Ok(changed)
    }

    pub fn promote_target(&mut self, raw: &str) -> Result<bool, ToolError> {
        let target = self.resolve_target(raw)?;
        let (dispatch_id, changed) = match target {
            Target::Dispatch(id) => {
                let record = self
                    .records
                    .get_mut(&id)
                    .ok_or_else(|| ToolError(format!("No dispatch '{raw}' in this session")))?;
                if !record.tasks.iter().any(|t| t.state.active()) {
                    return Err(ToolError(format!(
                        "{raw} is already settled; only an active dispatch can move to the background"
                    )));
                }
                (id, Self::promote_dispatch_record(record))
            }
            Target::Task {
                dispatch_id, index, ..
            } => {
                let record = self
                    .records
                    .get_mut(&dispatch_id)
                    .ok_or_else(|| ToolError("Dispatch disappeared".into()))?;
                let moved = Self::promote_task_in_record(record, index, raw)?;
                (dispatch_id, moved)
            }
        };
        if changed {
            self.push_record_entry(&dispatch_id);
            self.revision = self.revision.wrapping_add(1);
        }
        Ok(changed)
    }

    pub fn promote_all(&mut self) -> usize {
        let ids = self.order.clone();
        let mut moved = 0;
        for id in ids {
            let changed = self
                .records
                .get_mut(&id)
                .is_some_and(Self::promote_dispatch_record);
            if changed {
                self.push_record_entry(&id);
                moved += 1;
            }
        }
        if moved > 0 {
            self.revision = self.revision.wrapping_add(1);
        }
        moved
    }

    fn suppress_notifications_for(&mut self, predicate: impl Fn(&Notification) -> bool) {
        let before = self.notifications.len();
        self.notifications.retain(|n| !predicate(n));
        if self.notifications.len() != before {
            self.notification_seq = self.notification_seq.wrapping_add(1);
            self.revision = self.revision.wrapping_add(1);
        }
    }

    fn stop_task_entry(
        &mut self,
        task_id: &str,
        cause: StopCause,
    ) -> Option<watch::Receiver<bool>> {
        let owned = task_id.to_owned();
        self.suppress_notifications_for(|n| n.task_id == owned);
        let entry = self.tasks.get_mut(task_id)?;
        entry.notified = true;
        if entry.stop.is_none() {
            entry.stop = Some(cause);
        }
        entry.cancel.cancel();
        let dispatch_id = entry.dispatch_id.clone();
        let done = entry.done.clone();
        if let Some(disp) = self.dispatches.get_mut(&dispatch_id)
            && disp.mode == "chain"
        {
            disp.notified = true;
            if disp.stop.is_none() {
                disp.stop = Some(cause);
            }
            disp.cancel.cancel();
            return Some(disp.done.clone());
        }
        Some(done)
    }

    fn stop_dispatch_entry(
        &mut self,
        dispatch_id: &str,
        cause: StopCause,
    ) -> Vec<watch::Receiver<bool>> {
        let owned = dispatch_id.to_owned();
        self.suppress_notifications_for(|n| n.dispatch_id == owned);
        let mut waiters = Vec::new();
        if let Some(disp) = self.dispatches.get_mut(dispatch_id) {
            disp.notified = true;
            if disp.stop.is_none() {
                disp.stop = Some(cause);
            }
            disp.cancel.cancel();
            waiters.push(disp.done.clone());
        }
        let task_ids: Vec<String> = self
            .tasks
            .iter()
            .filter(|(_, t)| t.dispatch_id == dispatch_id)
            .map(|(id, _)| id.clone())
            .collect();
        for task_id in task_ids {
            if let Some(done) = self.stop_task_entry(&task_id, cause) {
                waiters.push(done);
            }
        }
        waiters
    }

    pub fn stop_target(
        &mut self,
        raw: &str,
        cause: StopCause,
    ) -> Result<(Target, Vec<watch::Receiver<bool>>), ToolError> {
        let target = self.resolve_target(raw)?;
        let waiters = match &target {
            Target::Task { task_id, .. } => {
                self.stop_task_entry(task_id, cause).into_iter().collect()
            }
            Target::Dispatch(id) => self.stop_dispatch_entry(id, cause),
        };
        if !waiters.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        Ok((target, waiters))
    }

    pub fn interrupt_foreground_dispatch(&mut self, dispatch_id: &str) {
        let Some(record) = self.records.get(dispatch_id) else {
            return;
        };
        if record.background {
            return;
        }
        let fg_ids: Vec<String> = record
            .tasks
            .iter()
            .filter(|t| !t.background && t.state.active())
            .map(|t| t.id.clone())
            .collect();
        if fg_ids.is_empty() {
            return;
        }
        if record.mode == "chain" {
            let _ = self.stop_dispatch_entry(dispatch_id, StopCause::Turn);
        } else {
            for id in fg_ids {
                let _ = self.stop_task_entry(&id, StopCause::Turn);
            }
        }
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn interrupt_foreground_all(&mut self) {
        let ids: Vec<String> = self
            .records
            .iter()
            .filter(|(_, r)| !r.background)
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            self.interrupt_foreground_dispatch(&id);
        }
    }

    pub fn stop_all(&mut self, cause: StopCause) -> Vec<watch::Receiver<bool>> {
        if cause == StopCause::Session {
            self.notifications.clear();
        }
        let ids: Vec<String> = self.dispatches.keys().cloned().collect();
        let mut waiters = Vec::new();
        for id in ids {
            waiters.extend(self.stop_dispatch_entry(&id, cause));
        }
        if !waiters.is_empty() {
            self.revision = self.revision.wrapping_add(1);
        }
        waiters
    }

    pub fn mark_dispatch_seen(&mut self, dispatch_id: &str) {
        let Some(record) = self.records.get(dispatch_id) else {
            return;
        };
        let settled_ids: Vec<String> = record
            .tasks
            .iter()
            .filter(|t| !t.state.active())
            .map(|t| t.id.clone())
            .collect();
        for id in &settled_ids {
            if let Some(entry) = self.tasks.get_mut(id) {
                entry.notified = true;
            }
        }
        self.suppress_notifications_for(|n| settled_ids.contains(&n.task_id));
    }

    pub fn queue_task_notification(&mut self, dispatch_id: &str, index: usize) {
        let Some(record) = self.records.get(dispatch_id) else {
            return;
        };
        if record.mode == "chain" {
            return;
        }
        let task = &record.tasks[index];
        if !task.background || !matches!(task.state, AgentState::Completed | AgentState::Failed) {
            return;
        }
        let Some(entry) = self.tasks.get_mut(&task.id) else {
            return;
        };
        if entry.notified || entry.stop.is_some() {
            return;
        }
        entry.notified = true;
        let xml = build_task_xml(&record.id, task);
        self.push_notification(task.id.clone(), record.id.clone(), xml);
    }

    pub fn queue_chain_notification(&mut self, dispatch_id: &str) {
        let Some(record) = self.records.get(dispatch_id) else {
            return;
        };
        if record.mode != "chain" || !record.background {
            return;
        }
        let Some(disp) = self.dispatches.get_mut(dispatch_id) else {
            return;
        };
        if disp.notified || disp.stop.is_some() {
            return;
        }
        disp.notified = true;
        if let Some((task_id, xml)) = build_chain_xml(record) {
            let record_id = record.id.clone();
            self.push_notification(task_id, record_id, xml);
        }
    }

    fn push_notification(&mut self, task_id: String, dispatch_id: String, xml: String) {
        self.notification_seq = self.notification_seq.wrapping_add(1);
        let key = format!("{task_id}#{}", self.notification_seq);
        self.notifications.push(Notification {
            key,
            task_id,
            dispatch_id,
            xml,
        });
        self.revision = self.revision.wrapping_add(1);
    }

    pub fn continuation(&self) -> Option<Continuation> {
        if self.notifications.is_empty() {
            return None;
        }
        let identity = self
            .notifications
            .iter()
            .map(|n| n.key.as_str())
            .collect::<Vec<_>>()
            .join(",");
        let text = self
            .notifications
            .iter()
            .map(|n| n.xml.as_str())
            .collect::<Vec<_>>()
            .join("\n\n");
        Some(Continuation {
            kind: CONTINUATION.into(),
            generation: self.notification_seq,
            identity,
            text,
            visible: false,
        })
    }

    pub fn delivered(&mut self, identity: &str) {
        let before = self.notifications.len();
        self.notifications
            .retain(|n| !identity.split(',').any(|key| key == n.key));
        if self.notifications.len() != before {
            self.revision = self.revision.wrapping_add(1);
        }
    }
}

fn task_status_word(state: AgentState) -> &'static str {
    match state {
        AgentState::Completed => "completed",
        AgentState::Failed => "failed",
        _ => "cancelled",
    }
}

fn task_result_excerpt(task: &AgentTask) -> String {
    let preview = output::prefix(&task.output, output::PREVIEW_BYTES);
    match (&task.diagnostic, preview.is_empty()) {
        (Some(diag), true) => diag.clone(),
        (Some(diag), false) => format!("{preview}\n{diag}"),
        (None, true) => "(no output)".into(),
        (None, false) => preview,
    }
}

fn format_xml(
    task_id: &str,
    tool_use_id: &str,
    status: &str,
    summary: &str,
    result_text: &str,
    output_path: Option<&str>,
    (turns, usage): (u64, TokenUsage),
) -> String {
    let mut xml = format!(
        "<task-notification>\n<task-id>{task_id}</task-id>\n<tool-use-id>{tool_use_id}</tool-use-id>\n<status>{status}</status>\n<summary>{summary}</summary>\n<result>{result_text}</result>\n"
    );
    if let Some(path) = output_path {
        xml.push_str(&format!("<output-file>{path}</output-file>\n"));
    }
    xml.push_str(&format!(
        "<usage><turns>{turns}</turns><input_tokens>{}</input_tokens><output_tokens>{}</output_tokens></usage>\n</task-notification>",
        usage.input, usage.output
    ));
    xml
}

fn build_task_xml(dispatch_id: &str, task: &AgentTask) -> String {
    let status = task_status_word(task.state);
    let label = match &task.name {
        Some(name) => format!("{name} · {}", task.agent),
        None => task.agent.clone(),
    };
    let summary = format!("Agent \"{}\" ({label}) {status}", task.id);
    format_xml(
        &task.id,
        dispatch_id,
        status,
        &summary,
        &task_result_excerpt(task),
        task.output_path.as_deref(),
        (task.turns, task.usage),
    )
}

fn build_chain_xml(record: &AgentDispatch) -> Option<(String, String)> {
    let total_turns: u64 = record.tasks.iter().map(|t| t.turns).sum();
    let total_usage = record
        .tasks
        .iter()
        .fold(TokenUsage::default(), |mut acc, t| {
            acc.input += t.usage.input;
            acc.output += t.usage.output;
            acc
        });
    if record
        .tasks
        .iter()
        .all(|t| t.state == AgentState::Completed)
    {
        let last = record.tasks.last()?;
        let summary = format!("Agent \"{}\" ({}) completed", last.id, last.agent);
        let xml = format_xml(
            &last.id,
            &record.id,
            "completed",
            &summary,
            &task_result_excerpt(last),
            last.output_path.as_deref(),
            (total_turns, total_usage),
        );
        return Some((last.id.clone(), xml));
    }
    let failed = record
        .tasks
        .iter()
        .find(|t| t.state == AgentState::Failed)?;
    let summary = format!("Agent \"{}\" ({}) failed", failed.id, failed.agent);
    let xml = format_xml(
        &failed.id,
        &record.id,
        "failed",
        &summary,
        &task_result_excerpt(failed),
        failed.output_path.as_deref(),
        (total_turns, total_usage),
    );
    Some((failed.id.clone(), xml))
}
