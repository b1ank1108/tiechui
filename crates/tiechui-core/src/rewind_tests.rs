use super::*;
use tiechui_plugin_api::{Module, ModuleConfig};

async fn restart(h: &mut Harness, directory: PathBuf) {
    h.shutdown.cancel();
    (&mut h.handle.task).await.unwrap().unwrap();
    let store = SessionStore::open(&h.path, &CancellationToken::new()).unwrap();
    let mut registry = Registry::default();
    tiechui_module_rewind::RewindModule { directory }
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let (tx, requests) = mpsc::unbounded_channel();
    h.requests = requests;
    let provider = Arc::new(FakeProvider {
        scripts: Mutex::new(VecDeque::new()),
        requests: tx,
    });
    let (interactions, inbox) = Interactions::new();
    h.interactions = interactions;
    h.epoch.0 += 1;
    h.shutdown = CancellationToken::new();
    h.handle = SessionActor::spawn(
        store,
        SessionConfig {
            compaction: Default::default(),
            model: origin("a"),
            thinking: None,
            providers: BTreeMap::from([("fake".into(), provider as Arc<dyn ModelProvider>)]),
            registry,
            retry: Arc::new(BoundedRetry::default()),
            interactions: Some(inbox),
            epoch: h.epoch,
        },
        h.shutdown.clone(),
    );
    h.until(|event| matches!(event, EventKind::ThreadStarted { .. }))
        .await;
}

struct PartialWrite(ToolSpec);
#[async_trait]
impl Tool for PartialWrite {
    fn spec(&self) -> &ToolSpec {
        &self.0
    }
    fn concurrency(&self, _: &serde_json::Value) -> Concurrency {
        Concurrency::Exclusive
    }
    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let path = tiechui_plugin_api::resolve_tool_path(
            call.arguments["path"].as_str().unwrap(),
            &ctx.cwd,
        )?;
        std::fs::write(path, "partial write")?;
        ctx.cancel.cancel();
        Ok(ToolOutput {
            is_error: true,
            ..ToolOutput::text("interrupted write")
        })
    }
}

#[tokio::test]
async fn cancelled_partial_writes_settle_control_anchored_checkpoints_with_editing_paths() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("work");
    std::fs::create_dir(&cwd).unwrap();
    let mut registry = Registry::default();
    let directory = root.path().join("rewind");
    tiechui_module_rewind::RewindModule {
        directory: directory.clone(),
    }
    .register(&mut registry, &ModuleConfig::default())
    .unwrap();
    registry.register_tool(Arc::new(PartialWrite(ToolSpec { name: "write".into(), description: String::new(), input_schema: json!({"type":"object","properties":{"path":{"type":"string"}},"required":["path"]}) }))).unwrap();
    let file_url = format!("file://{}/url.txt", cwd.display());
    let absolute = cwd.join("absolute.txt").to_str().unwrap().to_owned();
    let outside = root
        .path()
        .join("absolute-outside.txt")
        .to_str()
        .unwrap()
        .to_owned();
    for (input, file) in [
        ("@mention.txt", "mention.txt"),
        ("@@literal.txt", "@literal.txt"),
        ("@中文\u{a0}one\u{202f}two.txt", "中文 one two.txt"),
        (&file_url, "url.txt"),
        (&absolute, "absolute.txt"),
        ("../relative-outside.txt", "../relative-outside.txt"),
        (&outside, "../absolute-outside.txt"),
    ] {
        std::fs::write(cwd.join(file), "before").unwrap();
        let ctx = ToolContext {
            invoker: None,
            cwd: cwd.clone(),
            cancel: CancellationToken::new(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(|_| Ok(())),
            epoch: SessionEpoch(0),
            scope: ExecutionScope {
                thread_id: ThreadId("session".into()),
                turn_id: TurnId("turn".into()),
            },
            parent_session: None,
            branch: Arc::new(vec![BranchEntry {
                id: "control".into(),
                parent_id: None,
                timestamp: 0,
                content: None,
                custom: Some((
                    tiechui_module_rewind::CONTROL_CHECKPOINT.into(),
                    json!({"version":1,"text":"control operation"}),
                )),
            }]),
            origin: Default::default(),
        };
        let output = interception::execute_call(
            &registry,
            &ToolScheduler::default(),
            ToolCall {
                id: file.into(),
                name: "write".into(),
                arguments: json!({"path":input}),
                opaque: None,
            },
            ctx,
        )
        .await;
        assert!(output.is_error);
        assert_eq!(output.content[0], text_content("interrupted write".into()));
    }
    let store = tiechui_module_rewind::store::Store::new(&directory, &cwd, "session").unwrap();
    let cancel = CancellationToken::new();
    let plan = store.plan(&["control".into()], &cancel).unwrap();
    assert_eq!(plan.target.len(), 7);
    let _guard = store.restore(&plan, &cancel).unwrap();
    store.finish(&plan.target).unwrap();
    assert_eq!(
        std::fs::read_to_string(cwd.join("mention.txt")).unwrap(),
        "before"
    );
    for file in ["relative-outside.txt", "absolute-outside.txt"] {
        assert_eq!(
            std::fs::read_to_string(root.path().join(file)).unwrap(),
            "before"
        );
    }
}

#[tokio::test]
async fn external_edits_checkpoint_only_after_permission_allows_them() {
    use tiechui_module_permissions::{HeadlessPolicy, PermissionsModule};

    for allowed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let directory = root.path().join("rewind");
        let file = root.path().join("outside/nested/file.txt");
        let mut registry = Registry::default();
        tiechui_module_editing::EditingModule
            .register(&mut registry, &ModuleConfig::default())
            .unwrap();
        tiechui_module_rewind::RewindModule {
            directory: directory.clone(),
        }
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
        let mut config = ModuleConfig::default();
        config.permissions.rules = Some(serde_json::from_value(json!([
            {"surface":"*","pattern":"*","action":"allow"},
            {"surface":"external_directory_write","pattern":"*","action":if allowed { "allow" } else { "deny" }}
        ])).unwrap());
        PermissionsModule::headless(cwd.clone(), root.path().into(), HeadlessPolicy::Unavailable)
            .register(&mut registry, &config)
            .unwrap();
        let ctx = ToolContext {
            invoker: None,
            cwd: cwd.clone(),
            cancel: CancellationToken::new(),
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(|_| Ok(())),
            epoch: SessionEpoch(0),
            scope: ExecutionScope {
                thread_id: ThreadId("session".into()),
                turn_id: TurnId("turn".into()),
            },
            parent_session: None,
            branch: Arc::new(vec![BranchEntry {
                id: "prompt".into(),
                parent_id: None,
                timestamp: 0,
                content: Some(vec![InputContent::Text {
                    text: "create then edit".into(),
                }]),
                custom: None,
            }]),
            origin: Default::default(),
        };
        let output = interception::execute_call(
            &registry,
            &ToolScheduler::default(),
            ToolCall {
                id: "create".into(),
                name: "write".into(),
                arguments: json!({"path":"../outside/nested/file.txt","content":"original\n"}),
                opaque: None,
            },
            ctx.clone(),
        )
        .await;
        let store = tiechui_module_rewind::store::Store::new(&directory, &cwd, "session").unwrap();
        if !allowed {
            assert!(output.is_error, "{output:?}");
            assert!(
                matches!(&output.content[0], Content::Text { text, .. } if text.contains("Denied by policy"))
            );
            assert!(!file.exists());
            assert!(!store.directory.exists());
            continue;
        }
        assert!(!output.is_error, "{output:?}");
        let output = interception::execute_call(
            &registry,
            &ToolScheduler::default(),
            ToolCall {
                id: "edit".into(),
                name: "edit".into(),
                arguments: json!({"path":format!("file://{}", file.display()),"edits":[{"oldText":"original","newText":"edited"}]}),
                opaque: None,
            },
            ctx,
        ).await;
        assert!(!output.is_error, "{output:?}");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "edited\n");
        let cancel = CancellationToken::new();
        let plan = store.plan(&["prompt".into()], &cancel).unwrap();
        let key = std::fs::canonicalize(&file)
            .unwrap()
            .to_str()
            .unwrap()
            .to_owned();
        assert_eq!(plan.target.keys().collect::<Vec<_>>(), [&key]);
        assert!(plan.target[&key].is_none());
        let _lock = store.restore(&plan, &cancel).unwrap();
        store.finish(&plan.target).unwrap();
        assert!(!file.exists());
    }
}

#[tokio::test]
async fn image_only_refill_survives_restart_and_saved_removal_is_durable() {
    let storage = tempfile::tempdir().unwrap();
    let directory = storage.path().join("rewind");
    let mut h = Harness::with_contributions(
        vec![answer("image")],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_rewind::RewindModule {
                directory: directory.clone(),
            }
            .register(registry, &ModuleConfig::default())
            .unwrap();
        },
    )
    .await;
    let content = vec![InputContent::Image {
        mime_type: "image/png".into(),
        data: "cG5n".into(),
    }];
    h.send(Op::UserInput {
        content: content.clone(),
        delivery: InputDelivery::FollowUp,
    })
    .await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    let (id, rows) = open(&mut h).await;
    choose(&mut h, id, &rows.rows[0], "preview").await;
    let (id, _) = panel(&mut h, RewindPage::Actions).await;
    choose(&mut h, id, &rows.rows[0], "conversation").await;
    let done = h
        .until(|event| matches!(event, EventKind::CommandOutput { .. }))
        .await;
    h.epoch = done.epoch;
    restart(&mut h, directory.clone()).await;
    assert_eq!(
        h.until(|event| matches!(event, EventKind::ComposerRestored { .. }))
            .await
            .event,
        EventKind::ComposerRestored { content }
    );
    h.send(Op::ClearDraft).await;
    // A following command provides an acknowledgement of the preceding save.
    h.send(Op::RunCommand {
        name: "rewind".into(),
        arguments: "bad".into(),
    })
    .await;
    h.until(|event| matches!(event, EventKind::Error { .. }))
        .await;
    restart(&mut h, directory).await;
    let from = h.seen.len();
    h.until(|event| matches!(event, EventKind::CommandsUpdated { .. }))
        .await;
    assert!(
        !h.seen[from..]
            .iter()
            .any(|event| matches!(event.event, EventKind::ComposerRestored { .. }))
    );

    h.stop().await;
}

#[tokio::test]
async fn interrupted_restore_is_offered_directly_after_restart_and_cancel_keeps_journal() {
    let storage = tempfile::tempdir().unwrap();
    let directory = storage.path().join("rewind");
    let mut h = Harness::with_contributions(
        vec![],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_rewind::RewindModule {
                directory: directory.clone(),
            }
            .register(registry, &ModuleConfig::default())
            .unwrap();
        },
    )
    .await;
    let session = h.snapshot().await;
    let store = tiechui_module_rewind::store::Store::new(
        &directory,
        &session.header().cwd,
        &session.header().id,
    )
    .unwrap();
    let cancel = CancellationToken::new();
    std::fs::write(store.root.join("a"), "original").unwrap();
    store.before("one", "a", &cancel).unwrap();
    std::fs::write(store.root.join("a"), "changed").unwrap();
    store.after("a", &cancel).unwrap();
    let plan = store.plan(&["one".into()], &cancel).unwrap();
    drop(store.restore(&plan, &cancel).unwrap());
    restart(&mut h, directory).await;
    h.send(Op::RunCommand {
        name: "rewind".into(),
        arguments: String::new(),
    })
    .await;
    let (id, _) = panel(&mut h, RewindPage::Recovery).await;
    h.send(Op::ApprovalResponse {
        approval_id: id,
        decision: ApprovalDecision::Cancel,
    })
    .await;
    h.until(|event| matches!(event, EventKind::CommandOutput { .. }))
        .await;
    assert!(store.pending().unwrap());
    h.send(Op::RunCommand {
        name: "rewind".into(),
        arguments: String::new(),
    })
    .await;
    let (id, _) = panel(&mut h, RewindPage::Recovery).await;
    h.send(Op::ApprovalResponse {
        approval_id: id,
        decision: ApprovalDecision::Accept {
            content: Some(json!({"action":"recover"})),
        },
    })
    .await;
    h.until(|event| matches!(event, EventKind::CommandOutput { text, .. } if text.contains("Files recovered"))).await;
    assert_eq!(
        std::fs::read_to_string(store.root.join("a")).unwrap(),
        "changed"
    );
    assert!(!store.pending().unwrap());
    h.stop().await;
}

async fn panel(h: &mut Harness, page: RewindPage) -> (ApprovalId, RewindPanel) {
    let event = h.until(|event| matches!(event, EventKind::ApprovalRequested { request: ApprovalRequest::Rewind { panel }, .. } if panel.page == page)).await;
    let EventKind::ApprovalRequested {
        approval_id,
        request: ApprovalRequest::Rewind { panel },
        ..
    } = event.event
    else {
        unreachable!()
    };
    (approval_id, panel)
}
async fn choose(h: &mut Harness, id: ApprovalId, row: &RewindRow, action: &str) {
    h.send(Op::ApprovalResponse {
        approval_id: id,
        decision: ApprovalDecision::Accept {
            content: Some(
                json!({"session_id":row.session_id,"entry_id":row.entry_id,"action":action}),
            ),
        },
    })
    .await;
}
async fn open(h: &mut Harness) -> (ApprovalId, RewindPanel) {
    h.send(Op::RunCommand {
        name: "rewind".into(),
        arguments: String::new(),
    })
    .await;
    panel(h, RewindPage::Messages).await
}

#[tokio::test]
async fn text_only_refill_writes_no_draft_and_image_refill_is_saved_once_then_cleared_on_submit() {
    for images in [false, true] {
        let storage = tempfile::tempdir().unwrap();
        let mut h = Harness::with_contributions(
            vec![answer("done"), answer("submitted")],
            Arc::new(BoundedRetry::default()),
            json!({"type":"object"}),
            |registry| {
                tiechui_module_rewind::RewindModule {
                    directory: storage.path().join("rewind"),
                }
                .register(registry, &ModuleConfig::default())
                .unwrap();
            },
        )
        .await;
        let mut content = vec![InputContent::Text {
            text: "original prompt".into(),
        }];
        if images {
            content.push(InputContent::Image {
                mime_type: "image/png".into(),
                data: "cG5n".into(),
            });
        }
        h.send(Op::UserInput {
            content: content.clone(),
            delivery: InputDelivery::FollowUp,
        })
        .await;
        h.request().await;
        h.end(TurnStatus::Completed).await;
        let (id, rows) = open(&mut h).await;
        choose(&mut h, id, &rows.rows[0], "preview").await;
        let (id, _) = panel(&mut h, RewindPage::Actions).await;
        choose(&mut h, id, &rows.rows[0], "conversation").await;
        let done = h
            .until(|event| matches!(event, EventKind::CommandOutput { .. }))
            .await;
        h.epoch = done.epoch;
        let drafts = |store: &SessionStore| {
            store
                .entries()
                .iter()
                .filter_map(|entry| match &entry.kind {
                    EntryKind::Custom { kind, data } if kind == "tiechui.composer.draft" => {
                        Some(data.clone())
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        let saved = h.snapshot().await;
        assert_eq!(
            drafts(&saved),
            if images { vec![json!(content)] } else { vec![] }
        );
        assert!(
            !saved
                .build_context()
                .iter()
                .any(|message| message.role == Role::User)
        );
        h.send(Op::Reload).await;
        let started = h
            .until(|event| matches!(event, EventKind::ThreadStarted { .. }))
            .await;
        h.epoch = started.epoch;
        assert!(matches!(
            started.event,
            EventKind::ThreadStarted { replay: false, .. }
        ));
        h.until(|event| matches!(event, EventKind::CommandsUpdated { .. }))
            .await;
        assert_eq!(
            drafts(&h.snapshot().await),
            drafts(&saved),
            "reload must not append drafts"
        );
        // After navigation, reload must retain the selected parent even without a draft entry.
        assert!(
            !h.snapshot()
                .await
                .build_context()
                .iter()
                .any(|message| message.role == Role::User)
        );
        content[0] = InputContent::Text {
            text: "edited prompt".into(),
        };
        h.send(Op::UserInput {
            content,
            delivery: InputDelivery::FollowUp,
        })
        .await;
        h.request().await;
        h.end(TurnStatus::Completed).await;
        let final_drafts = drafts(&h.snapshot().await);
        assert_eq!(final_drafts.len(), if images { 2 } else { 0 });
        if images {
            assert!(final_drafts[1].is_null());
        }
        h.send(Op::Reload).await;
        let from = h.seen.len();
        let done = h
            .until(|event| matches!(event, EventKind::CommandsUpdated { .. }))
            .await;
        h.epoch = done.epoch;
        assert!(
            !h.seen[from..]
                .iter()
                .any(|event| matches!(event.event, EventKind::ComposerRestored { .. }))
        );
        h.stop().await;
    }
}

#[tokio::test]
async fn all_restore_modes_keep_old_branch_and_refill_images_across_reload() {
    for action in ["both", "conversation", "code"] {
        let storage = tempfile::tempdir().unwrap();
        let directory = storage.path().join("rewind");
        let mut h = Harness::with_contributions(
            vec![
                reply(vec![Content::ToolCall(ToolCall {
                    id: "write".into(),
                    name: "write".into(),
                    arguments: json!({"path":"@a.txt","content":"changed\n"}),
                    opaque: None,
                })]),
                answer("done"),
            ],
            Arc::new(BoundedRetry::default()),
            json!({"type":"object"}),
            |registry| {
                tiechui_module_editing::EditingModule
                    .register(registry, &ModuleConfig::default())
                    .unwrap();
                tiechui_module_rewind::RewindModule { directory }
                    .register(registry, &ModuleConfig::default())
                    .unwrap();
            },
        )
        .await;
        let cwd = h.snapshot().await.header().cwd.clone();
        std::fs::write(cwd.join("a.txt"), "original\n").unwrap();
        let content = vec![
            InputContent::Text {
                text: "change file".into(),
            },
            InputContent::Image {
                mime_type: "image/png".into(),
                data: "cG5n".into(),
            },
        ];
        h.send(Op::UserInput {
            content: content.clone(),
            delivery: InputDelivery::FollowUp,
        })
        .await;
        h.request().await;
        h.request().await;
        h.end(TurnStatus::Completed).await;
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap(),
            "changed\n"
        );
        let original = h.snapshot().await;
        let (id, list) = open(&mut h).await;
        assert_eq!(list.rows.len(), 1);
        // The whole command owns the interaction slot and cannot start another turn.
        h.input("must not start", InputDelivery::FollowUp).await;
        h.until(|event| matches!(event, EventKind::Error { message } if message.contains("Finish or cancel"))).await;
        choose(&mut h, id, &list.rows[0], "preview").await;
        let (id, preview) = panel(&mut h, RewindPage::Actions).await;
        assert_eq!(preview.preview.as_ref().unwrap().files.len(), 1);
        choose(&mut h, id, &list.rows[0], action).await;
        let done = h
            .until(
                |event| matches!(event, EventKind::CommandOutput { name, .. } if name == "rewind"),
            )
            .await;
        h.epoch = done.epoch;
        assert!(h.requests.try_recv().is_err());
        assert_eq!(
            std::fs::read_to_string(cwd.join("a.txt")).unwrap(),
            if action == "conversation" {
                "changed\n"
            } else {
                "original\n"
            }
        );
        let saved = h.snapshot().await;
        assert!(
            original
                .entries()
                .iter()
                .all(|entry| saved.entries().contains(entry))
        );
        if action != "code" {
            assert!(
                !saved
                    .build_context()
                    .iter()
                    .any(|message| message.role == Role::User)
            );
            assert!(h.seen.iter().any(|event| matches!(&event.event, EventKind::ComposerRefilled { content: restored } if restored == &content)));
            h.send(Op::Reload).await;
            let refill = h
                .until(|event| matches!(event, EventKind::ComposerRestored { .. }))
                .await;
            h.epoch = refill.epoch;
            assert_eq!(
                refill.event,
                EventKind::ComposerRestored {
                    content: content.clone()
                }
            );
            h.send(Op::RunCommand {
                name: "rewind".into(),
                arguments: "detach".into(),
            })
            .await;
            h.until(|event| matches!(event, EventKind::CommandOutput { .. }))
                .await;
            let from = h.seen.len();
            h.send(Op::Reload).await;
            let done = h
                .until(|event| matches!(event, EventKind::CommandsUpdated { .. }))
                .await;
            h.epoch = done.epoch;
            assert!(
                !h.seen[from..]
                    .iter()
                    .any(|event| matches!(event.event, EventKind::ComposerRestored { .. }))
            );
        } else {
            assert!(
                saved
                    .build_context()
                    .iter()
                    .any(|message| message.role == Role::User)
            );
        }
        h.stop().await;
    }
}

#[tokio::test]
async fn conflicts_leave_conversation_action_and_stale_confirmation_cannot_restore() {
    let storage = tempfile::tempdir().unwrap();
    let directory = storage.path().join("rewind");
    let mut h = Harness::with_contributions(
        vec![answer("done")],
        Arc::new(BoundedRetry::default()),
        json!({"type":"object"}),
        |registry| {
            tiechui_module_rewind::RewindModule {
                directory: directory.clone(),
            }
            .register(registry, &ModuleConfig::default())
            .unwrap();
        },
    )
    .await;
    h.input("first", InputDelivery::FollowUp).await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    let store = h.snapshot().await;
    let user = store.entries().iter().find(|entry| matches!(&entry.kind, EntryKind::Message { message, .. } if message.role == Role::User)).unwrap();
    let files = tiechui_module_rewind::store::Store::new(
        &directory,
        &store.header().cwd,
        &store.header().id,
    )
    .unwrap();
    let cancel = CancellationToken::new();
    std::fs::write(files.root.join("a"), "original").unwrap();
    files.before(&user.id, "a", &cancel).unwrap();
    std::fs::write(files.root.join("a"), "new").unwrap();
    files.after("a", &cancel).unwrap();
    std::fs::write(files.root.join("a"), "external").unwrap();
    let (id, rows) = open(&mut h).await;
    choose(&mut h, id, &rows.rows[0], "preview").await;
    let (id, preview) = panel(&mut h, RewindPage::Actions).await;
    assert!(preview.preview.unwrap().error.unwrap().contains("External"));
    choose(&mut h, id.clone(), &rows.rows[0], "back").await;
    let (new_id, _) = panel(&mut h, RewindPage::Messages).await;
    choose(&mut h, id, &rows.rows[0], "both").await;
    choose(&mut h, new_id, &rows.rows[0], "preview").await;
    let (id, _) = panel(&mut h, RewindPage::Actions).await;
    choose(&mut h, id, &rows.rows[0], "conversation").await;
    h.until(|event| matches!(event, EventKind::CommandOutput { .. }))
        .await;
    assert_eq!(
        std::fs::read_to_string(files.root.join("a")).unwrap(),
        "external"
    );
    h.stop().await;
}
