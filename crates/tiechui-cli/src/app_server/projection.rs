//! One session's state as its events leave it, so a client that connects late, or again,
//! starts from a snapshot instead of the session's whole history.

use std::collections::BTreeMap;

use tiechui_protocol::app::LiveState;
use tiechui_protocol::{ApprovalId, Event, EventKind, Item, ItemId, KnownItem, SessionEpoch};

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
}

impl Projection {
    pub fn state(&self) -> LiveState {
        if !self.approvals.is_empty() {
            LiveState::Waiting
        } else if !self.turn.is_empty() {
            LiveState::Running
        } else if self.retry {
            LiveState::Failed
        } else {
            LiveState::Idle
        }
    }

    /// Whether a turn, an approval, a compaction or queued input is under way.
    pub fn busy(&self) -> bool {
        let queued = matches!(
            self.latest.get("queue").map(|e| &e.event),
            Some(EventKind::QueueUpdated { steering, follow_up })
                if !steering.is_empty() || !follow_up.is_empty()
        );
        queued
            || !self.turn.is_empty()
            || !self.approvals.is_empty()
            || self.latest.contains_key("compaction")
    }

    /// Numbers the event and folds it in.
    pub fn apply(&mut self, event: &Event) -> u64 {
        self.seq += 1;
        self.epoch = event.epoch;
        match &event.event {
            EventKind::ThreadStarted { .. } => {
                self.thread = Some(event.clone());
                self.turn.clear();
                self.approvals.clear();
                self.error = None;
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
            EventKind::ModelsUpdated { .. } => self.keep("models", event),
            EventKind::CommandsUpdated { .. } => self.keep("commands", event),
            EventKind::ResourcesUpdated { .. } => self.keep("resources", event),
            EventKind::QueueUpdated { .. } => self.keep("queue", event),
            EventKind::Extension { name, .. } => self.keep(&format!("extension:{name}"), event),
            _ => {}
        }
        self.seq
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
}
