use super::*;
use tiechui_plugin_api::{BranchView, RuntimeEvent};

impl SessionActor {
    pub(super) async fn inherit_runtime(&mut self) -> Result<(), String> {
        if self.registry.runtime_extensions().is_empty() {
            return Ok(());
        }
        let store = self.store()?;
        if store.entries().iter().any(|entry| matches!(&entry.kind, EntryKind::Custom { kind, data } if kind == "session.runtime" && data.as_str() == Some(&self.thread.0))) { return Ok(()); }
        if let Some(path) = store.header().parent_session.clone() {
            let cancel = self.shutdown.child_token();
            let entries = tokio::task::spawn_blocking(move || {
                let store =
                    SessionStore::open_read_only(&path, &cancel).map_err(|e| e.to_string())?;
                Ok::<_, String>(
                    store
                        .path_to_root(store.current_leaf().unwrap_or(&store.header().id))
                        .map_err(|e| e.to_string())?
                        .iter()
                        .map(|entry| BranchEntry {
                            id: entry.id.clone(),
                            parent_id: entry.parent_id.clone(),
                            timestamp: entry.timestamp,
                            content: None,
                            custom: match &entry.kind {
                                EntryKind::Custom { kind, data } => {
                                    Some((kind.clone(), data.clone()))
                                }
                                _ => None,
                            },
                        })
                        .collect(),
                )
            })
            .await
            .map_err(|e| e.to_string())?;
            let entries = match entries {
                Ok(entries) => entries,
                Err(message) => {
                    self.emit(EventKind::Error {
                        message: format!("Module state inheritance failed: {message}"),
                    });
                    vec![]
                }
            };
            self.runtime(RuntimeEvent::Fork { entries }).await?;
        }
        self.append(EntryKind::Custom {
            kind: "session.runtime".into(),
            data: serde_json::json!(self.thread.0),
        })
        .await?;
        Ok(())
    }

    pub(super) fn branch_view(&self) -> Result<BranchView, String> {
        let store = self.store()?;
        Ok(BranchView {
            session_id: self.thread.0.clone(),
            leaf: store.current_leaf().unwrap_or(&store.header().id).into(),
            cwd: store.header().cwd.clone(),
            parent_session: store.header().parent_session.clone(),
            entries: self.branch_entries()?,
        })
    }

    pub(super) async fn runtime(&mut self, event: RuntimeEvent) -> Result<(), String> {
        if self.registry.runtime_extensions().is_empty() {
            return Ok(());
        }
        let branch = self.branch_view()?;
        for extension in self.registry.clone().runtime_extensions() {
            let output = extension.event(&event, &branch).map_err(|e| e.0)?;
            for (kind, data) in output.entries {
                self.append(EntryKind::Custom { kind, data }).await?;
            }
            for (name, data) in output.events {
                self.emit(EventKind::Extension { name, data });
            }
        }
        self.publish_resources();
        Ok(())
    }

    /// Stops background work owned by this session and waits for it to settle.
    pub(super) async fn quiesce(&self) {
        for work in self.registry.background_work() {
            work.quiesce().await;
        }
    }

    pub(super) async fn run_turn(&mut self, submission: Submission) -> Result<(), String> {
        self.runtime(RuntimeEvent::Input {
            continuation: self.automatic_input.clone(),
            retry: matches!(submission.op, Op::Retry),
        })
        .await?;
        self.settled = None;
        let result = self.run_turn_inner(submission).await;
        self.automatic_input = None;
        if let Some(status) = self.settled.take() {
            self.runtime(RuntimeEvent::Settled(status)).await?;
        }
        result
    }

    pub(super) async fn continue_idle(&mut self) -> Result<(), String> {
        if self.input_blocked
            || self.compacting
            || self.pending_compaction.is_some()
            || self.turn.is_some()
            || self.prompts.head().is_some()
            || !self.ops.is_empty()
            || !self.steering.is_empty()
            || !self.follow_up.is_empty()
            || self.navigation.is_some()
            || self.shutdown.is_cancelled()
        {
            return Ok(());
        }
        let registry = self.registry.clone();
        let extensions = registry.runtime_extensions();
        for offset in 0..extensions.len() {
            let index = (self.next_extension + offset) % extensions.len();
            let extension = &extensions[index];
            if let Some(next) = extension.continuation() {
                let branch = self.branch_view()?;
                let epoch = self.epoch;
                // Validate at consumption, after all higher-priority actor work.
                if epoch != self.epoch
                    || branch.session_id != self.thread.0
                    || branch.leaf != self.branch_view()?.leaf
                    || !self.ops.is_empty()
                    || !extension.accepts(&next)
                {
                    return Ok(());
                }
                self.next_extension = index + 1;
                let content = vec![InputContent::Text {
                    text: next.text.clone(),
                }];
                self.automatic_input = Some(next);
                self.run_turn(Submission {
                    id: SubmissionId(format!("internal-{}", self.sequence + 1)),
                    thread_id: self.thread.clone(),
                    epoch,
                    op: Op::UserInput {
                        content,
                        delivery: InputDelivery::FollowUp,
                    },
                })
                .await?;
                if let Some(navigation) = self.navigation.take() {
                    self.control(navigation).await?;
                }
                break;
            }
        }
        Ok(())
    }
}
