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
            short_cache: false,
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

fn test_tool_context(cwd: PathBuf, branch: Vec<BranchEntry>) -> ToolContext {
    ToolContext {
        invoker: None,
        cwd,
        cancel: CancellationToken::new(),
        updates: Arc::new(|_| {}),
        append_entry: Arc::new(|_| Ok(())),
        epoch: SessionEpoch(0),
        scope: ExecutionScope {
            thread_id: ThreadId("session".into()),
            turn_id: TurnId("turn".into()),
        },
        parent_session: None,
        branch: Arc::new(branch),
        origin: Default::default(),
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
        let ctx = test_tool_context(
            cwd.clone(),
            vec![BranchEntry {
                id: "control".into(),
                parent_id: None,
                timestamp: 0,
                content: None,
                custom: Some((
                    tiechui_module_rewind::CONTROL_CHECKPOINT.into(),
                    json!({"version":1,"text":"control operation"}),
                )),
            }],
        );
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

fn editing_rewind_fixture(
    root: &std::path::Path,
    cwd: &std::path::Path,
    directory: &std::path::Path,
    rules: serde_json::Value,
    prompt_text: &str,
) -> (Registry, ToolContext) {
    use tiechui_module_permissions::{HeadlessPolicy, PermissionsModule};

    let mut registry = Registry::default();
    tiechui_module_editing::EditingModule
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    tiechui_module_rewind::RewindModule {
        directory: directory.to_path_buf(),
    }
    .register(&mut registry, &ModuleConfig::default())
    .unwrap();
    let mut config = ModuleConfig::default();
    config.permissions.rules = Some(serde_json::from_value(rules).unwrap());
    PermissionsModule::headless(cwd.to_path_buf(), root.into(), HeadlessPolicy::Unavailable)
        .register(&mut registry, &config)
        .unwrap();
    let ctx = test_tool_context(
        cwd.to_path_buf(),
        vec![BranchEntry {
            id: "prompt".into(),
            parent_id: None,
            timestamp: 0,
            content: Some(vec![InputContent::Text {
                text: prompt_text.into(),
            }]),
            custom: None,
        }],
    );
    (registry, ctx)
}

#[tokio::test]
async fn external_edits_checkpoint_only_after_permission_allows_them() {
    for allowed in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let cwd = root.path().join("work");
        std::fs::create_dir(&cwd).unwrap();
        let directory = root.path().join("rewind");
        let file = root.path().join("outside/nested/file.txt");
        let (registry, ctx) = editing_rewind_fixture(
            root.path(),
            &cwd,
            &directory,
            json!([
                {"surface":"*","pattern":"*","action":"allow"},
                {"surface":"external_directory_write","pattern":"*","action":if allowed { "allow" } else { "deny" }}
            ]),
            "create then edit",
        );
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
        assert_eq!(plan.target.len(), 1);
        let (key, version) = plan.target.first_key_value().unwrap();
        assert_eq!(
            std::fs::canonicalize(key).unwrap(),
            std::fs::canonicalize(&file).unwrap()
        );
        assert!(version.is_none());
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

#[tokio::test]
async fn upper_repo_writes_to_exempted_agent_working_dirs_succeed_and_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let cwd = root.path().join("repo");
    std::fs::create_dir(&cwd).unwrap();
    let directory = root.path().join("rewind");
    let (registry, ctx) = editing_rewind_fixture(
        root.path(),
        &cwd,
        &directory,
        json!([{"surface":"*","pattern":"*","action":"allow"}]),
        "write worktree files",
    );

    let targets = [
        ".tiechui/worktrees/x/a.txt",
        ".tiechui/plans/plan.md",
        ".tiechui/state/run.json",
        ".claude/worktrees/x/a.txt",
        ".claude/plans/plan.md",
        ".claude/state/run.json",
        ".agents/worktrees/x/a.txt",
        ".agents/plans/plan.md",
        ".agents/state/run.json",
    ];
    for (idx, rel) in targets.iter().enumerate() {
        let output = interception::execute_call(
            &registry,
            &ToolScheduler::default(),
            ToolCall {
                id: format!("w-{idx}"),
                name: "write".into(),
                arguments: json!({"path": rel, "content": format!("content-{idx}\n")}),
                opaque: None,
            },
            ctx.clone(),
        )
        .await;
        assert!(!output.is_error, "{rel}: {output:?}");
        assert_eq!(
            std::fs::read_to_string(cwd.join(rel)).unwrap(),
            format!("content-{idx}\n")
        );
    }

    let store = tiechui_module_rewind::store::Store::new(&directory, &cwd, "session").unwrap();
    let cancel = CancellationToken::new();
    let plan = store.plan(&["prompt".into()], &cancel).unwrap();
    assert_eq!(plan.target.len(), targets.len());
    let _lock = store.restore(&plan, &cancel).unwrap();
    store.finish(&plan.target).unwrap();
    for rel in targets {
        assert!(!cwd.join(rel).exists(), "{rel}");
    }
}

fn run_git_test(root: &std::path::Path, args: &[&str]) -> String {
    let mut cmd = std::process::Command::new("git");
    cmd.current_dir(root).args(args);
    for (k, v) in [
        ("GIT_AUTHOR_NAME", "Tiechui"),
        ("GIT_AUTHOR_EMAIL", "tiechui@example.com"),
        ("GIT_COMMITTER_NAME", "Tiechui"),
        ("GIT_COMMITTER_EMAIL", "tiechui@example.com"),
    ] {
        cmd.env(k, v);
    }
    let output = cmd.output().expect("spawn git");
    if !output.status.success() {
        panic!("git {args:?}: {}", String::from_utf8_lossy(&output.stderr));
    }
    String::from_utf8(output.stdout).expect("utf-8 git stdout")
}

fn setup_managed_worktree_fixture(
    temp: &tempfile::TempDir,
) -> (PathBuf, PathBuf, PathBuf, PathBuf) {
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    init_worktree_test_repo(&repo);
    let wt =
        tiechui_workspace::create_worktree_with_env(&home, &repo, None, &BTreeMap::new()).unwrap();
    let wt_path = PathBuf::from(wt.path);
    let rewind_dir = home.join("state/rewind");
    (home, repo, wt_path, rewind_dir)
}

fn init_worktree_test_repo(repo: &std::path::Path) {
    std::fs::create_dir_all(repo).unwrap();
    run_git_test(repo, &["init", "-b", "main"]);
    std::fs::write(repo.join("a.txt"), "initial-a\n").unwrap();
    std::fs::write(repo.join("staged.txt"), "initial-staged\n").unwrap();
    std::fs::write(repo.join(".gitignore"), "node_modules/\n").unwrap();
    run_git_test(repo, &["add", "."]);
    run_git_test(repo, &["commit", "-m", "init"]);
}

fn user_entry_ids(store: &SessionStore) -> Vec<String> {
    store
        .entries()
        .iter()
        .filter_map(|entry| match &entry.kind {
            EntryKind::Message { message, .. } if message.role == Role::User => {
                Some(entry.id.clone())
            }
            _ => None,
        })
        .collect()
}

fn register_worktree_rewind_stack(
    registry: &mut Registry,
    rewind_dir: PathBuf,
    options: tiechui_module_rewind::SnapshotOptions,
) {
    tiechui_module_editing::EditingModule
        .register(registry, &ModuleConfig::default())
        .unwrap();
    tiechui_module_exec::ExecModule { background: false }
        .register(registry, &ModuleConfig::default())
        .unwrap();
    tiechui_module_rewind::RewindModule {
        directory: rewind_dir,
    }
    .register_with_options(registry, &ModuleConfig::default(), options)
    .unwrap();
}

#[tokio::test]
async fn managed_worktree_turns_capture_hidden_tree_snapshots_before_bash_mutations() {
    let temp = tempfile::tempdir().unwrap();
    let (home, repo, wt_path, rewind_dir) = setup_managed_worktree_fixture(&temp);

    std::fs::write(wt_path.join("staged.txt"), "user-staged\n").unwrap();
    run_git_test(&wt_path, &["add", "staged.txt"]);
    std::fs::write(wt_path.join("pre_existing.txt"), "untracked-v1\n").unwrap();
    std::fs::create_dir_all(wt_path.join("node_modules/pkg")).unwrap();
    std::fs::write(wt_path.join("node_modules/pkg/index.js"), "ignored\n").unwrap();

    let cached_before = run_git_test(&wt_path, &["diff", "--cached"]);
    let branches_before = run_git_test(&wt_path, &["branch", "-a"]);
    let log_before = run_git_test(&wt_path, &["log", "--all", "--oneline"]);

    let mut h = Harness::with_workspace(
        temp,
        home.clone(),
        wt_path.clone(),
        vec![
            answer("turn-1-ok"),
            reply(vec![Content::ToolCall(ToolCall {
                id: "bash-1".into(),
                name: "bash".into(),
                arguments: json!({
                    "command": "rm a.txt && printf 'x\\n' > b.txt && printf 'untracked-v2\\n' > pre_existing.txt"
                }),
                opaque: None,
            })]),
            answer("turn-2-ok"),
        ],
        |registry| {
            register_worktree_rewind_stack(registry, rewind_dir.clone(), Default::default());
        },
    )
    .await;

    h.input("inspect", InputDelivery::FollowUp).await;
    h.request().await;
    h.end(TurnStatus::Completed).await;

    h.input("mutate with bash", InputDelivery::FollowUp).await;
    h.request().await;
    h.request().await;
    h.end(TurnStatus::Completed).await;

    assert!(!wt_path.join("a.txt").exists());
    assert_eq!(
        std::fs::read_to_string(wt_path.join("b.txt")).unwrap(),
        "x\n"
    );
    assert_eq!(
        std::fs::read_to_string(wt_path.join("pre_existing.txt")).unwrap(),
        "untracked-v2\n"
    );
    assert_eq!(run_git_test(&wt_path, &["diff", "--cached"]), cached_before);
    assert_eq!(run_git_test(&wt_path, &["branch", "-a"]), branches_before);
    assert_eq!(
        run_git_test(&wt_path, &["log", "--all", "--oneline"]),
        log_before
    );

    let saved = h.snapshot().await;
    let anchors = user_entry_ids(&saved);
    assert_eq!(anchors.len(), 2);
    let rewind_store =
        tiechui_module_rewind::store::Store::new(&rewind_dir, &wt_path, &saved.header().id)
            .unwrap();
    let snap2 = rewind_store.snapshot(&anchors[1]).unwrap();
    let Some(tiechui_module_rewind::store::SnapshotRecord::Ready { reference, tree }) = snap2
    else {
        panic!("expected Ready snapshot for turn 2, got {snap2:?}");
    };
    assert!(matches!(
        rewind_store.snapshot(&anchors[0]).unwrap(),
        Some(tiechui_module_rewind::store::SnapshotRecord::Ready { .. })
    ));
    assert!(reference.starts_with("refs/tiechui/checkpoints/"));

    run_git_test(&repo, &["gc", "--prune=now", "-q"]);
    run_git_test(&repo, &["fsck", "--full"]);
    assert_eq!(
        run_git_test(&wt_path, &["show", &format!("{tree}:a.txt")]),
        "initial-a\n"
    );
    assert_eq!(
        run_git_test(&wt_path, &["show", &format!("{tree}:pre_existing.txt")]),
        "untracked-v1\n"
    );
    let tree_listing = run_git_test(&wt_path, &["ls-tree", "-r", "--name-only", &tree]);
    assert!(!tree_listing.lines().any(|line| line == "b.txt"));
    assert!(
        !tree_listing
            .lines()
            .any(|line| line.starts_with("node_modules/"))
    );
    h.stop().await;
}

async fn run_single_turn_in_dir(
    cwd: PathBuf,
) -> BTreeMap<String, tiechui_module_rewind::store::SnapshotRecord> {
    let home_holder = tempfile::tempdir().unwrap();
    let home = home_holder.path().join("home");
    let rewind_dir = home.join("state/rewind");
    let mut h = Harness::with_workspace(
        home_holder,
        home,
        cwd.clone(),
        vec![answer("done")],
        |registry| {
            register_worktree_rewind_stack(registry, rewind_dir.clone(), Default::default());
        },
    )
    .await;
    h.input("hello", InputDelivery::FollowUp).await;
    h.request().await;
    h.end(TurnStatus::Completed).await;
    let session_id = h.snapshot().await.header().id.clone();
    h.stop().await;
    tiechui_module_rewind::store::Store::new(&rewind_dir, &cwd, &session_id)
        .unwrap()
        .snapshots()
        .unwrap()
}

#[tokio::test]
async fn non_managed_workspaces_do_not_capture_hidden_git_snapshots() {
    let temp = tempfile::tempdir().unwrap();
    let repo = temp.path().join("project-root");
    init_worktree_test_repo(&repo);
    let manual_wt = temp.path().join("manual-wt");
    run_git_test(
        &repo,
        &[
            "worktree",
            "add",
            "-b",
            "manual/branch",
            manual_wt.to_str().unwrap(),
            "main",
        ],
    );
    let plain = temp.path().join("plain-dir");
    std::fs::create_dir_all(&plain).unwrap();

    assert!(run_single_turn_in_dir(repo.clone()).await.is_empty());
    assert!(run_single_turn_in_dir(manual_wt.clone()).await.is_empty());
    assert!(run_single_turn_in_dir(plain).await.is_empty());
    assert_eq!(
        run_git_test(&repo, &["for-each-ref", "refs/tiechui/checkpoints/"]),
        ""
    );
    assert_eq!(
        run_git_test(&manual_wt, &["for-each-ref", "refs/tiechui/checkpoints/"]),
        ""
    );
}

#[cfg(unix)]
fn write_executable_script(dir: &std::path::Path, name: &str, body: &str) -> PathBuf {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir).unwrap();
    let path = dir.join(name);
    std::fs::write(&path, body).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

#[cfg(unix)]
async fn run_turn_with_snapshot_options(
    options: tiechui_module_rewind::SnapshotOptions,
) -> (tiechui_module_rewind::store::SnapshotRecord, bool) {
    let temp = tempfile::tempdir().unwrap();
    let (home, _repo, wt_path, rewind_dir) = setup_managed_worktree_fixture(&temp);

    let mut h = Harness::with_workspace(
        temp,
        home,
        wt_path.clone(),
        vec![
            reply(vec![Content::ToolCall(ToolCall {
                id: "write-after-fail".into(),
                name: "write".into(),
                arguments: json!({"path": "created.txt", "content": "ok\n"}),
                opaque: None,
            })]),
            answer("completed despite snapshot failure"),
        ],
        |registry| {
            register_worktree_rewind_stack(registry, rewind_dir.clone(), options);
        },
    )
    .await;
    h.input("proceed anyway", InputDelivery::FollowUp).await;
    h.request().await;
    h.request().await;
    h.end(TurnStatus::Completed).await;

    let had_error = h
        .seen
        .iter()
        .any(|event| matches!(event.event, EventKind::Error { .. }));
    assert_eq!(
        std::fs::read_to_string(wt_path.join("created.txt")).unwrap(),
        "ok\n"
    );
    let saved = h.snapshot().await;
    let anchors = user_entry_ids(&saved);
    let store = tiechui_module_rewind::store::Store::new(&rewind_dir, &wt_path, &saved.header().id)
        .unwrap();
    let record = store
        .snapshot(&anchors[0])
        .unwrap()
        .expect("expected missing snapshot record");
    h.stop().await;
    (record, had_error)
}

#[cfg(unix)]
#[tokio::test]
async fn snapshot_timeout_and_failure_do_not_block_managed_worktree_turns() {
    let scripts_dir = tempfile::tempdir().unwrap();
    let orig_path = std::env::var("PATH").unwrap_or_default();

    let slow_bin = scripts_dir.path().join("slow-bin");
    write_executable_script(&slow_bin, "git", "#!/bin/sh\nsleep 5\n");
    let (timeout_record, timeout_err) =
        run_turn_with_snapshot_options(tiechui_module_rewind::SnapshotOptions {
            timeout: Duration::from_millis(80),
            env: BTreeMap::from([("PATH".into(), format!("{}:{orig_path}", slow_bin.display()))]),
        })
        .await;
    assert!(!timeout_err);
    assert!(
        matches!(&timeout_record, tiechui_module_rewind::store::SnapshotRecord::Missing { reason } if reason.contains("timed out")),
        "unexpected timeout record: {timeout_record:?}"
    );

    let fail_bin = scripts_dir.path().join("fail-bin");
    write_executable_script(
        &fail_bin,
        "git",
        "#!/bin/sh\necho 'simulated git failure' >&2\nexit 1\n",
    );
    let (fail_record, fail_err) =
        run_turn_with_snapshot_options(tiechui_module_rewind::SnapshotOptions {
            timeout: Duration::from_secs(2),
            env: BTreeMap::from([("PATH".into(), format!("{}:{orig_path}", fail_bin.display()))]),
        })
        .await;
    assert!(!fail_err);
    assert!(
        matches!(&fail_record, tiechui_module_rewind::store::SnapshotRecord::Missing { reason } if reason.contains("simulated git failure")),
        "unexpected fail record: {fail_record:?}"
    );
}
