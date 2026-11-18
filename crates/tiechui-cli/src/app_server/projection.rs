//! One session's state as its events leave it, so a client that connects late, or again,
//! starts from a snapshot instead of the session's whole history.

use std::collections::{BTreeMap, HashMap};

use serde_json::Value;
use tiechui_protocol::app::LiveState;
use tiechui_protocol::{
    ApprovalId, ApprovalRequest, BTW_EVENT, BtwEvent, BtwView, Event, EventKind, Item, ItemId,
    KnownItem, SessionEpoch, SubmissionId, TurnId,
};

use super::textgen::InitialTitleTrigger;

#[derive(Default)]
pub(super) struct Projection {
    pub seq: u64,
    pub epoch: SessionEpoch,
    /// The latest `ThreadStarted`, extended by each completed turn.
    thread: Option<Event>,
    /// Events of the turn in progress; streamed text is merged, completed items replace
    /// their partial events, and tool progress keeps only the latest update.
    turn: Vec<Event>,
    /// Kinds whose latest event is the state, by kind name or extension name.
    latest: BTreeMap<String, Event>,
    approvals: Vec<(ApprovalId, Event)>,
    /// The latest error since a turn started; it explains the open or last failed turn.
    error: Option<Event>,
    retry: bool,
    side_busy_id: Option<String>,
    tracked_submissions: HashMap<SubmissionId, u32>,
    initial_title: InitialTitleTrigger,
}

impl Projection {
    pub fn state(&self) -> LiveState {
        if self.has_main_approval() {
            LiveState::Waiting
        } else if !self.turn.is_empty() {
            LiveState::Running
        } else if self.retry {
            LiveState::Failed
        } else {
            LiveState::Idle
        }
    }

    fn has_main_approval(&self) -> bool {
        self.approvals.iter().any(|(_, event)| {
            !matches!(
                &event.event,
                EventKind::ApprovalRequested {
                    turn_id: Some(turn_id),
                    ..
                } if turn_id.0.starts_with("side-")
            )
        })
    }

    /// Whether a turn, a side reply, an approval, a compaction or queued input is under way.
    pub fn busy(&self) -> bool {
        let queued = matches!(
            self.latest.get("queue").map(|e| &e.event),
            Some(EventKind::QueueUpdated { steering, follow_up })
                if !steering.is_empty() || !follow_up.is_empty()
        );
        queued
            || !self.turn.is_empty()
            || !self.approvals.is_empty()
            || self.side_busy()
            || self.latest.contains_key("compaction")
    }

    pub(super) fn side_busy(&self) -> bool {
        self.side_busy_id.is_some()
    }

    pub(super) fn btw_view(&self) -> Option<BtwView> {
        let EventKind::Extension { data, .. } = &self.latest.get("extension:btw")?.event else {
            return None;
        };
        match serde_json::from_value::<BtwEvent>(data.clone()).ok()? {
            BtwEvent::Update { view, .. } => Some(view),
            _ => None,
        }
    }

    pub(super) fn has_command(&self, name: &str) -> bool {
        match self.latest.get("commands").map(|e| &e.event) {
            Some(EventKind::CommandsUpdated { commands }) => {
                commands.iter().any(|cmd| cmd.name == name)
            }
            _ => true,
        }
    }

    pub(super) fn track_submission(&mut self, id: SubmissionId) {
        self.tracked_submissions.insert(id, 0);
    }

    pub(super) fn submission_count(&self, id: &SubmissionId) -> u32 {
        self.tracked_submissions.get(id).copied().unwrap_or(0)
    }

    pub(super) fn untrack_submission(&mut self, id: &SubmissionId) {
        self.tracked_submissions.remove(id);
    }

    pub(super) fn normalize_event(&self, event: &mut Event) {
        let EventKind::ApprovalRequested {
            turn_id, request, ..
        } = &mut event.event
        else {
            return;
        };
        if let ApprovalRequest::Permission { call_id, .. } = request
            && let Some(side_turn) = side_turn_from_call_id(call_id)
        {
            *turn_id = Some(side_turn);
        } else if turn_id.is_none()
            && self.turn.is_empty()
            && matches!(
                request,
                ApprovalRequest::Questions { .. } | ApprovalRequest::Elicitation { .. }
            )
            && let Some(side_id) = &self.side_busy_id
        {
            *turn_id = Some(TurnId(side_id.clone()));
        }
    }

    fn bump_tracked(&mut self, event: &Event) {
        if let Some(sub_id) = &event.submission_id
            && let Some(count) = self.tracked_submissions.get_mut(sub_id)
        {
            *count += 1;
        }
    }

    /// Numbers the event and folds it in.
    pub fn apply(&mut self, event: &Event) -> u64 {
        self.seq += 1;
        self.epoch = event.epoch;
        self.bump_tracked(event);
        match &event.event {
            EventKind::ThreadStarted { .. } => {
                self.thread = Some(event.clone());
                self.turn.clear();
                self.approvals.clear();
                self.error = None;
                self.side_busy_id = None;
                self.latest.remove("extension:btw");
            }
            EventKind::TurnStarted { .. } => {
                self.turn = vec![event.clone()];
                self.error = None;
            }
            EventKind::Error { .. } => self.error = Some(event.clone()),
            EventKind::ItemStarted { .. } => self.turn.push(event.clone()),
            EventKind::ItemDelta { item_id, delta, .. } => self.delta(event, item_id, delta),
            EventKind::ItemCompleted { item, .. } => {
                let id = item_id(item);
                self.turn.retain(|e| match &e.event {
                    EventKind::ItemStarted { item, .. } => item_id(item) != id,
                    EventKind::ItemDelta { item_id, .. } => Some(item_id) != id.as_ref(),
                    _ => true,
                });
                self.turn.push(event.clone());
            }
            EventKind::TurnCompleted { turn } => {
                self.turn.clear();
                if let Some(Event {
                    event: EventKind::ThreadStarted { thread, .. },
                    ..
                }) = &mut self.thread
                {
                    thread.turns.push(turn.clone());
                }
            }
            EventKind::ApprovalRequested { approval_id, .. } => {
                self.approvals.push((approval_id.clone(), event.clone()));
            }
            EventKind::ApprovalResolved { approval_id } => {
                self.approvals.retain(|(id, _)| id != approval_id);
            }
            EventKind::RetryAvailable { available } => {
                self.retry = *available;
                self.keep("retry", event);
            }
            EventKind::CompactionStarted { .. } | EventKind::CompactionProgress { .. } => {
                self.keep("compaction", event);
            }
            EventKind::CompactionEnded { .. } => {
                self.latest.remove("compaction");
            }
            EventKind::SessionUpdated { .. } => self.keep("session", event),
            EventKind::TokenUsageUpdated { last, total, .. } => {
                if let Some(Event {
                    event:
                        EventKind::SessionUpdated {
                            context,
                            total: kept,
                            ..
                        },
                    ..
                }) = self.latest.get_mut("session")
                {
                    *context = Some(*last);
                    *kept = *total;
                }
            }
            EventKind::ModelsUpdated { .. } => self.keep("models", event),
            EventKind::CommandsUpdated { .. } => self.keep("commands", event),
            EventKind::ResourcesUpdated { .. } => self.keep("resources", event),
            EventKind::QueueUpdated { .. } => self.keep("queue", event),
            EventKind::Extension { name, data } => self.apply_extension(name, data, event),
            _ => {}
        }
        self.seq
    }

    fn apply_extension(&mut self, name: &str, data: &Value, event: &Event) {
        if name != BTW_EVENT {
            self.keep(&format!("extension:{name}"), event);
            return;
        }
        match serde_json::from_value::<BtwEvent>(data.clone()) {
            Ok(BtwEvent::Update { view, .. }) => {
                self.side_busy_id = view.busy.then_some(view.id);
                self.keep("extension:btw", event);
            }
            Ok(BtwEvent::Closed) => {
                self.side_busy_id = None;
                self.latest.remove("extension:btw");
            }
            Ok(BtwEvent::Models { .. }) => {}
            Err(_) => self.keep("extension:btw", event),
        }
    }

    /// Events that rebuild the current state, in an order a client can apply.
    pub fn snapshot(&self) -> Vec<Event> {
        self.thread
            .iter()
            .chain(self.latest.values())
            .chain(&self.turn)
            .chain(&self.error)
            .chain(self.approvals.iter().map(|(_, event)| event))
            .cloned()
            .collect()
    }

    pub(super) fn take_initial_title_trigger(&mut self, event: &Event) -> Option<(String, String)> {
        self.initial_title.take(event)
    }

    fn keep(&mut self, key: &str, event: &Event) {
        self.latest.insert(key.into(), event.clone());
    }

    /// Text and tool-call arguments stream as appended pieces; tool progress replaces.
    fn delta(&mut self, event: &Event, id: &ItemId, delta: &str) {
        let appends = self.turn.iter().any(|e| {
            matches!(&e.event, EventKind::ItemStarted { item: Item::Known(
                KnownItem::AssistantMessage { id: started, .. }
                | KnownItem::Thinking { id: started, .. }
                | KnownItem::ToolCall { id: started, .. },
            ), .. } if started == id)
        });
        let previous = self.turn.iter_mut().rev().find_map(|e| match &mut e.event {
            EventKind::ItemDelta { item_id, delta, .. } if item_id == id => Some(delta),
            _ => None,
        });
        match previous {
            Some(previous) if appends => previous.push_str(delta),
            Some(previous) => *previous = delta.into(),
            None => self.turn.push(event.clone()),
        }
    }
}

fn side_turn_from_call_id(call_id: &str) -> Option<TurnId> {
    let rest = call_id.strip_prefix("command-side-")?;
    let digits_len = rest.bytes().take_while(u8::is_ascii_digit).count();
    if digits_len == 0 || rest.as_bytes().get(digits_len) != Some(&b'-') {
        return None;
    }
    Some(TurnId(format!("side-{}", &rest[..digits_len])))
}

pub(super) fn is_btw_settled(kind: &EventKind) -> bool {
    let EventKind::Extension { name, data } = kind else {
        return false;
    };
    name == BTW_EVENT
        && matches!(
            serde_json::from_value::<BtwEvent>(data.clone()),
            Ok(BtwEvent::Closed)
                | Ok(BtwEvent::Update {
                    view: BtwView { busy: false, .. },
                    ..
                })
        )
}

fn item_id(item: &Item) -> Option<ItemId> {
    match item {
        Item::Known(
            KnownItem::Compaction { id, .. }
            | KnownItem::UserMessage { id, .. }
            | KnownItem::AssistantMessage { id, .. }
            | KnownItem::Thinking { id, .. }
            | KnownItem::ToolCall { id, .. }
            | KnownItem::ToolResult { id, .. },
        ) => Some(id.clone()),
        Item::Custom { payload, .. } => payload
            .get("id")
            .and_then(|id| id.as_str())
            .map(|id| ItemId(id.into())),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;
    use tiechui_protocol::{ApprovalRequest, Thread, ThreadId, Turn, TurnId, TurnStatus};

    fn event(kind: EventKind) -> Event {
        Event {
            submission_id: None,
            thread_id: ThreadId("t".into()),
            epoch: SessionEpoch(1),
            event: kind,
        }
    }

    fn turn(status: TurnStatus, items: Vec<Item>) -> Turn {
        Turn {
            id: TurnId("turn".into()),
            status,
            items,
            started_at: 1,
            ended_at: None,
        }
    }

    fn text(id: &str, text: &str) -> Item {
        Item::Known(KnownItem::AssistantMessage {
            id: ItemId(id.into()),
            text: text.into(),
        })
    }

    fn delta(id: &str, delta: &str) -> EventKind {
        EventKind::ItemDelta {
            turn_id: TurnId("turn".into()),
            item_id: ItemId(id.into()),
            delta: delta.into(),
        }
    }

    #[test]
    fn snapshot_merges_text_replaces_progress_and_tracks_state() {
        let mut p = Projection::default();
        p.apply(&event(EventKind::ThreadStarted {
            thread: Thread {
                id: ThreadId("t".into()),
                epoch: SessionEpoch(1),
                turns: vec![],
            },
            replay: false,
        }));
        p.apply(&event(EventKind::QueueUpdated {
            steering: vec![],
            follow_up: vec![],
        }));
        p.apply(&event(EventKind::TurnStarted {
            turn: turn(TurnStatus::InProgress, vec![]),
        }));
        p.apply(&event(EventKind::ItemStarted {
            turn_id: TurnId("turn".into()),
            item: text("a", ""),
        }));
        p.apply(&event(delta("a", "hel")));
        p.apply(&event(delta("a", "lo")));
        p.apply(&event(delta("r", "{\"n\":1}")));
        p.apply(&event(delta("r", "{\"n\":2}")));
        assert_eq!(p.state(), LiveState::Running);
        let snapshot = p.snapshot();
        assert_eq!(snapshot.len(), 6);
        assert!(
            matches!(&snapshot[4].event, EventKind::ItemDelta { delta, .. } if delta == "hello")
        );
        assert!(
            matches!(&snapshot[5].event, EventKind::ItemDelta { delta, .. } if delta == "{\"n\":2}")
        );

        p.apply(&event(EventKind::ItemCompleted {
            turn_id: TurnId("turn".into()),
            item: text("a", "hello"),
        }));
        assert_eq!(p.snapshot().len(), 5);
        p.apply(&event(EventKind::ApprovalRequested {
            turn_id: None,
            approval_id: ApprovalId("x".into()),
            request: ApprovalRequest::Information {
                title: "t".into(),
                text: "t".into(),
            },
        }));
        assert_eq!(p.state(), LiveState::Waiting);
        p.apply(&event(EventKind::ApprovalResolved {
            approval_id: ApprovalId("x".into()),
        }));
        p.apply(&event(EventKind::Error {
            message: "boom".into(),
        }));
        p.apply(&event(EventKind::TurnCompleted {
            turn: turn(TurnStatus::Failed, vec![text("a", "hello")]),
        }));
        let seq = p.apply(&event(EventKind::RetryAvailable { available: true }));
        assert_eq!((seq, p.state()), (14, LiveState::Failed));
        let snapshot = p.snapshot();
        assert_eq!(snapshot.len(), 4);
        assert!(
            matches!(&snapshot[0].event, EventKind::ThreadStarted { thread, .. } if thread.turns.len() == 1)
        );
        assert_eq!(
            serde_json::to_value(&snapshot[2].event).unwrap(),
            json!({"kind": "retry_available", "available": true})
        );
        assert!(matches!(&snapshot[3].event, EventKind::Error { message } if message == "boom"));
    }

    #[test]
    fn snapshot_session_carries_the_latest_usage() {
        use tiechui_protocol::{ModelInfo, ThinkingLevel, TokenUsage};

        let usage = |input| TokenUsage {
            input,
            output: input / 50,
            ..Default::default()
        };
        let mut p = Projection::default();
        p.apply(&event(EventKind::SessionUpdated {
            model: ModelInfo {
                provider: "fixture".into(),
                id: "model".into(),
                context_window: 200000,
                reasoning: false,
                thinking_levels: vec![ThinkingLevel::Off],
            },
            thinking: ThinkingLevel::Off,
            context: None,
            total: TokenUsage::default(),
        }));
        for total in [100, 200] {
            p.apply(&event(EventKind::TokenUsageUpdated {
                turn_id: TurnId("turn".into()),
                last: usage(100),
                total: usage(total),
            }));
        }
        let snapshot = p.snapshot();
        assert!(matches!(
            &snapshot[..],
            [Event { event: EventKind::SessionUpdated { context: Some(context), total, .. }, .. }]
                if *context == usage(100) && *total == usage(200)
        ));
    }

    fn user(id: &str, parts: &[&str]) -> Item {
        Item::Known(KnownItem::UserMessage {
            id: ItemId(id.into()),
            content: parts
                .iter()
                .map(|part| tiechui_protocol::InputContent::Text {
                    text: (*part).into(),
                })
                .collect(),
        })
    }

    #[test]
    fn initial_title_trigger_extracts_opening_prompt_and_locks_once() {
        let empty_thread = || Thread {
            id: ThreadId("t".into()),
            epoch: SessionEpoch(1),
            turns: vec![],
        };

        let mut fresh = Projection::default();
        assert_eq!(
            fresh.take_initial_title_trigger(&event(EventKind::ThreadStarted {
                thread: empty_thread(),
                replay: false,
            })),
            None
        );
        assert_eq!(
            fresh.take_initial_title_trigger(&event(EventKind::TurnCompleted {
                turn: turn(TurnStatus::Failed, vec![user("u0", &["failed"])]),
            })),
            None
        );
        assert_eq!(
            fresh.take_initial_title_trigger(&event(EventKind::TurnCompleted {
                turn: turn(
                    TurnStatus::Completed,
                    vec![
                        user("u1", &["首轮主问题", "补充上下文"]),
                        text("a1", "先检查配置"),
                        user("u2", &["中途 steer 提示"]),
                        text("a2", "已定位根因"),
                    ]
                ),
            })),
            Some((
                "首轮主问题\n补充上下文".into(),
                "先检查配置\n\n已定位根因".into()
            ))
        );
        assert_eq!(
            fresh.take_initial_title_trigger(&event(EventKind::TurnCompleted {
                turn: turn(TurnStatus::Completed, vec![user("u3", &["第二轮"])]),
            })),
            None
        );

        let mut blank_first = Projection::default();
        blank_first.take_initial_title_trigger(&event(EventKind::ThreadStarted {
            thread: empty_thread(),
            replay: false,
        }));
        assert_eq!(
            blank_first.take_initial_title_trigger(&event(EventKind::TurnCompleted {
                turn: turn(
                    TurnStatus::Completed,
                    vec![user("u1", &["   "]), user("u2", &["hook 注入提示"])]
                ),
            })),
            None
        );
        assert_eq!(
            blank_first.take_initial_title_trigger(&event(EventKind::TurnCompleted {
                turn: turn(TurnStatus::Completed, vec![user("u3", &["第二轮"])]),
            })),
            None
        );

        let mut replayed = Projection::default();
        assert_eq!(
            replayed.take_initial_title_trigger(&event(EventKind::ThreadStarted {
                thread: empty_thread(),
                replay: true,
            })),
            None
        );
        assert_eq!(
            replayed.take_initial_title_trigger(&event(EventKind::ThreadStarted {
                thread: empty_thread(),
                replay: false,
            })),
            None
        );
        assert_eq!(
            replayed.take_initial_title_trigger(&event(EventKind::TurnCompleted {
                turn: turn(TurnStatus::Completed, vec![user("u1", &["不生成"])]),
            })),
            None
        );

        let mut resumed = Projection::default();
        assert_eq!(
            resumed.take_initial_title_trigger(&event(EventKind::ThreadStarted {
                thread: Thread {
                    turns: vec![turn(TurnStatus::Completed, vec![])],
                    ..empty_thread()
                },
                replay: false,
            })),
            None
        );
        assert_eq!(
            resumed.take_initial_title_trigger(&event(EventKind::TurnCompleted {
                turn: turn(TurnStatus::Completed, vec![user("u2", &["恢复会话"])]),
            })),
            None
        );
    }

    #[test]
    fn btw_projection_tracks_busy_preserves_view_and_scopes_side_approvals() {
        use tiechui_protocol::{ModelInfo, ThinkingLevel, TokenUsage};

        let side_view = |busy: bool| BtwView {
            id: "side-7".into(),
            model: ModelInfo {
                provider: "alpha".into(),
                id: "main".into(),
                context_window: 128000,
                reasoning: false,
                thinking_levels: vec![ThinkingLevel::Off],
            },
            thinking: ThinkingLevel::Off,
            messages: vec![],
            busy,
            context: None,
            total: TokenUsage::default(),
            notice: None,
        };

        let mut p = Projection::default();
        p.apply(&event(EventKind::ThreadStarted {
            thread: Thread {
                id: ThreadId("t".into()),
                epoch: SessionEpoch(1),
                turns: vec![],
            },
            replay: false,
        }));
        assert!(!p.busy());

        p.apply(&event(EventKind::Extension {
            name: BTW_EVENT.into(),
            data: serde_json::to_value(BtwEvent::Update {
                view: side_view(true),
                open: true,
            })
            .unwrap(),
        }));
        assert!(p.busy());
        assert_eq!(p.state(), LiveState::Idle);
        assert_eq!(p.btw_view().unwrap().id, "side-7");

        p.apply(&event(EventKind::Extension {
            name: BTW_EVENT.into(),
            data: serde_json::to_value(BtwEvent::Models {
                models: vec![],
                current: None,
            })
            .unwrap(),
        }));
        assert_eq!(p.btw_view().unwrap().id, "side-7");

        let mut side_perm = event(EventKind::ApprovalRequested {
            turn_id: Some(TurnId("turn-main".into())),
            approval_id: ApprovalId("a-side".into()),
            request: ApprovalRequest::Permission {
                actor: None,
                call_id: "command-side-7-call_w1".into(),
                tool: "write".into(),
                arguments: json!({}),
                surface: "write".into(),
                values: vec![],
                matched_pattern: None,
                origin: "direct".into(),
                session_patterns: vec![],
                preview: None,
            },
        });
        p.normalize_event(&mut side_perm);
        assert!(matches!(
            &side_perm.event,
            EventKind::ApprovalRequested { turn_id: Some(t), .. } if t.0 == "side-7"
        ));
        p.apply(&side_perm);
        assert_eq!(p.state(), LiveState::Idle);
        assert!(p.busy());

        p.apply(&event(EventKind::TurnStarted {
            turn: turn(TurnStatus::InProgress, vec![]),
        }));
        assert_eq!(p.state(), LiveState::Running);
        p.apply(&event(EventKind::TurnCompleted {
            turn: turn(TurnStatus::Completed, vec![]),
        }));
        p.apply(&event(EventKind::ApprovalResolved {
            approval_id: ApprovalId("a-side".into()),
        }));
        p.apply(&event(EventKind::Extension {
            name: BTW_EVENT.into(),
            data: serde_json::to_value(BtwEvent::Update {
                view: side_view(false),
                open: false,
            })
            .unwrap(),
        }));
        assert!(!p.busy());

        p.apply(&event(EventKind::Extension {
            name: BTW_EVENT.into(),
            data: serde_json::to_value(BtwEvent::Closed).unwrap(),
        }));
        assert!(p.btw_view().is_none());
    }
}
