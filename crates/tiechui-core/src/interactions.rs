//! Permission approvals and MCP elicitations wait in one queue; clients see only its head,
//! so prompts never overlap.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tiechui_protocol::{ApprovalDecision, ApprovalId, ApprovalRequest};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

pub(crate) enum Message {
    Ask(Box<Prompt>),
    Withdraw(ApprovalId),
}

/// The asking side, shared by the modules that prompt.
#[derive(Clone)]
pub struct Interactions {
    tx: mpsc::UnboundedSender<Message>,
    next: Arc<AtomicU64>,
}

/// The session side, handed to the actor through `SessionConfig`.
pub struct Inbox(pub(crate) mpsc::UnboundedReceiver<Message>);

impl Interactions {
    pub fn new() -> (Self, Inbox) {
        let (tx, rx) = mpsc::unbounded_channel();
        let interactions = Self {
            tx,
            next: Arc::default(),
        };
        (interactions, Inbox(rx))
    }

    /// The client's decision; `None` once cancelled or when the session is gone.
    pub async fn ask(
        &self,
        request: ApprovalRequest,
        cancel: CancellationToken,
    ) -> Option<ApprovalDecision> {
        let id = ApprovalId(format!(
            "approval-{}",
            self.next.fetch_add(1, Ordering::Relaxed) + 1
        ));
        let (reply, answer) = oneshot::channel();
        self.tx
            .send(Message::Ask(Box::new(Prompt {
                id: id.clone(),
                request,
                reply,
            })))
            .ok()?;
        // Withdraws on every exit, including a dropped future; answered ids are ignored.
        let _withdraw = Withdraw(&self.tx, id);
        tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            decision = answer => decision.ok(),
        }
    }
}

struct Withdraw<'a>(&'a mpsc::UnboundedSender<Message>, ApprovalId);

impl Drop for Withdraw<'_> {
    fn drop(&mut self) {
        let _ = self.0.send(Message::Withdraw(self.1.clone()));
    }
}

pub(crate) struct Prompt {
    pub id: ApprovalId,
    pub request: ApprovalRequest,
    pub(crate) reply: oneshot::Sender<ApprovalDecision>,
}

/// The actor's queue; the head is the request shown to the client.
#[derive(Default)]
pub(crate) struct Queue(VecDeque<Prompt>);

impl Queue {
    pub fn update(&mut self, id: &ApprovalId, request: ApprovalRequest) {
        if let Some(prompt) = self.0.iter_mut().find(|prompt| &prompt.id == id) {
            prompt.request = request;
        }
    }
    pub fn head(&self) -> Option<&Prompt> {
        self.0.front()
    }

    pub fn apply(&mut self, message: Message) {
        match message {
            Message::Ask(prompt) => self.0.push_back(*prompt),
            Message::Withdraw(id) => self.settle(&id, None),
        }
    }

    /// Answers the head only: a response to anything else is stale.
    pub fn answer(&mut self, id: &ApprovalId, decision: ApprovalDecision) {
        if self.head().is_some_and(|prompt| &prompt.id == id) {
            self.settle(id, Some(decision));
        }
    }

    /// Cancels everything, as when the thread changes.
    pub fn clear(&mut self) {
        for prompt in self.0.drain(..) {
            let _ = prompt.reply.send(ApprovalDecision::Cancel);
        }
    }

    fn settle(&mut self, id: &ApprovalId, decision: Option<ApprovalDecision>) {
        if let Some(index) = self.0.iter().position(|prompt| &prompt.id == id)
            && let Some(prompt) = self.0.remove(index)
            && let Some(decision) = decision
        {
            let _ = prompt.reply.send(decision);
        }
    }
}

/// The next message, or never once every asking side is gone.
pub(crate) async fn next(inbox: &mut Option<Inbox>) -> Option<Message> {
    match inbox {
        Some(Inbox(rx)) => rx.recv().await,
        None => std::future::pending().await,
    }
}
