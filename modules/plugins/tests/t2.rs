#![cfg(unix)]
#![cfg(feature = "test-server")]
#![allow(clippy::unwrap_used)]

use std::{
    fs,
    path::Path,
    sync::{Arc, Mutex},
    time::Duration,
};
use tiechui_module_plugins::{Extension, Plugin, PluginHost, discover};
use tiechui_plugin_api::{
    model::ToolCall,
    protocol::*,
    serde_json::json,
    tools::{ExecutionScope, ToolUpdate},
    *,
};

#[derive(Default)]
struct Host {
    prompts: Mutex<Vec<PluginPrompt>>,
    decline: bool,
}
#[async_trait]
impl PluginHost for Host {
    async fn ask(
        &self,
        _: &str,
        prompt: PluginPrompt,
        _: CancellationToken,
    ) -> Result<ApprovalDecision, ToolError> {
        self.prompts.lock().unwrap().push(prompt);
        Ok(if self.decline {
            ApprovalDecision::Decline
        } else {
            ApprovalDecision::Accept {
                content: Some(json!({"value":"First","answer":"yes"})),
            }
        })
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    plugin: Plugin,
    host: Arc<Host>,
}
impl Fixture {
    fn new(mode: &str) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/plugin-tests");
        fs::create_dir_all(&root).unwrap();
        let temp = tempfile::tempdir_in(root).unwrap();
        let package = temp.path().join("home/plugins/fixture");
        fs::create_dir_all(package.join(".tiechui-plugin")).unwrap();
        // Keep repeated startup hashes small; PTY cases cover the compiled binary.
        // exec preserves the host's process group.
        use std::os::unix::fs::PermissionsExt;
        let binary = env!("CARGO_BIN_EXE_t2-test-plugin").replace('\'', "'\\''");
        fs::write(
            package.join("server"),
            format!("#!/bin/sh\nexec '{binary}' \"$@\"\n"),
        )
        .unwrap();
        fs::set_permissions(package.join("server"), fs::Permissions::from_mode(0o700)).unwrap();
        fs::write(package.join(".tiechui-plugin/plugin.json"), json!({
            "name":"fixture", "tiechui":{"apiVersion":1},
            "extension":{"command":"server","args":[temp.path(), mode],
                "capabilities":["tools","commands","tool.intercept","ui.notify","ui.select","ui.confirm","ui.input","ui.panel","status.set"],
                "contributions":{
                    "tools":[{"name":"echo","description":"Example","inputSchema":{"type":"object"}}],
                    "commands":[{"name":"ui","description":"Open example UI"}],
                    "interceptors":["before_policy","policy","after_policy"]
                }}
        }).to_string()).unwrap();
        let plugin = discover(
            temp.path(),
            &temp.path().join("home"),
            true,
            &ModuleConfig::default(),
            &CancellationToken::new(),
        )
        .unwrap()
        .plugins
        .remove(0);
        assert!(plugin.diagnostics.is_empty(), "{:?}", plugin.diagnostics);
        Self {
            temp,
            plugin,
            host: Arc::default(),
        }
    }
    fn start(&self, host: Option<Arc<dyn PluginHost>>) -> (Extension, Registry, CancellationToken) {
        let cancel = CancellationToken::new();
        let extension = Extension::start(
            &self.plugin,
            &self.temp.path().join("home"),
            true,
            host,
            cancel.clone(),
        )
        .unwrap();
        let mut registry = Registry::default();
        extension
            .register(&mut registry, &ModuleConfig::default())
            .unwrap();
        (extension, registry, cancel)
    }
    fn branch(&self) -> BranchView {
        BranchView {
            session_id: "test".into(),
            leaf: "test".into(),
            cwd: self.temp.path().into(),
            parent_session: None,
            entries: vec![],
        }
    }
    fn context(
        &self,
        cancel: CancellationToken,
        updates: Arc<dyn Fn(ToolUpdate) + Send + Sync>,
    ) -> ToolContext {
        ToolContext {
            invoker: None,
            branch: Arc::default(),
            parent_session: None,
            cwd: self.temp.path().into(),
            cancel,
            updates,
            append_entry: Arc::new(|_| Ok(())),
            epoch: SessionEpoch(0),
            scope: ExecutionScope {
                thread_id: ThreadId("t".into()),
                turn_id: TurnId("turn".into()),
            },
            origin: Default::default(),
        }
    }
    fn pids(&self) -> Vec<i32> {
        fs::read_to_string(self.temp.path().join("pids"))
            .unwrap_or_default()
            .split_whitespace()
            .map(|s| s.parse().unwrap())
            .collect()
    }
    async fn stopped(&self) {
        let pids = self.pids();
        wait("process-group cleanup", || {
            pids.iter().all(|pid| exited(*pid))
        })
        .await;
    }
}

fn exited(pid: i32) -> bool {
    let missing = unsafe { libc::kill(pid, 0) } == -1
        && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH);
    // Orphans killed with their parent may await reaping by PID 1 on Linux.
    #[cfg(target_os = "linux")]
    let zombie = fs::read_to_string(format!("/proc/{pid}/stat")).is_ok_and(|stat| {
        stat.rsplit_once(") ")
            .is_some_and(|(_, fields)| fields.starts_with("Z "))
    });
    #[cfg(not(target_os = "linux"))]
    let zombie = false;
    missing || zombie
}

async fn wait(description: &str, mut predicate: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(12), async {
        while !predicate() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {description}"));
}
async fn ready(registry: &Registry) {
    wait("plugin readiness", || !registry.tools().specs().is_empty()).await;
}

#[cfg(target_os = "linux")]
#[tokio::test]
async fn cleanup_liveness_ignores_unreaped_zombies() {
    let mut child = std::process::Command::new("/bin/sh")
        .args(["-c", "exit 0"])
        .spawn()
        .unwrap();
    let pid = child.id() as i32;
    assert!(!exited(std::process::id() as i32));
    // std::process::Child leaves the zombie waitable until we explicitly reap it.
    wait("child zombie", || exited(pid)).await;
    assert_eq!(unsafe { libc::kill(pid, 0) }, 0);
    assert!(child.wait().unwrap().success());
    assert!(exited(pid));
}
fn call(action: &str) -> ToolCall {
    ToolCall {
        id: "call".into(),
        name: "plugin_fixture_echo".into(),
        arguments: json!({"action":action}),
        opaque: None,
    }
}

#[tokio::test]
async fn every_capability_registers_through_the_module_api() {
    let f = Fixture::new("normal");
    let (extension, registry, cancel) = f.start(Some(f.host.clone()));
    ready(&registry).await;
    let progress = Arc::new(Mutex::new(Vec::new()));
    let updates = progress.clone();
    let ctx = f.context(
        CancellationToken::new(),
        Arc::new(move |v| updates.lock().unwrap().push(v)),
    );
    let tool = registry.tools().get("plugin_fixture_echo").unwrap();
    assert!(tool.deferrable());
    assert_eq!(
        tool.run(call("echo"), ctx.clone()).await.unwrap(),
        ToolOutput::text("Plugin tool result")
    );
    assert_eq!(progress.lock().unwrap().len(), 1);
    assert_eq!(registry.interceptors().len(), 3);
    for interceptor in registry.interceptors() {
        assert_eq!(
            interceptor
                .interceptor
                .before(&call("deny"), ctx.clone())
                .await
                .unwrap(),
            BeforeToolCall::Deny("Denied by test plugin".into())
        );
        assert!(matches!(
            interceptor
                .interceptor
                .before(&call("rewrite"), ctx.clone())
                .await
                .unwrap(),
            BeforeToolCall::Rewrite(_)
        ));
        assert_eq!(
            interceptor
                .interceptor
                .after(&call("echo"), &ToolOutput::text("x"), ctx.clone())
                .await
                .unwrap(),
            None
        );
    }
    for argument in ["notify", "select", "confirm", "input", "panel", "status"] {
        assert_eq!(
            registry.commands()[0]
                .run(argument, CancellationToken::new())
                .await
                .unwrap(),
            "UI completed"
        );
    }
    let prompts = f.host.prompts.lock().unwrap().clone();
    assert_eq!(prompts.len(), 5); // Authorization, then the four interactive UI methods.
    assert!(matches!(&prompts[4], PluginPrompt::Panel { panel } if panel.nodes.len() == 5));
    let events = extension
        .event(&RuntimeEvent::Tick, &f.branch())
        .unwrap()
        .events;
    assert!(
        events
            .iter()
            .any(|(_, data)| data["text"] == "Fixture status")
    );
    assert!(events.iter().any(|(name, data)| name == PLUGIN_NOTICE_EVENT
        && data == "fixture: Notice: Plugin notification"));
    let responses = fs::read_to_string(f.temp.path().join("responses")).unwrap();
    assert_eq!(responses.lines().next(), Some(r#"{"result":null}"#));
    cancel.cancel();
    f.stopped().await;
}

#[tokio::test]
async fn cancellation_and_interceptor_deadline_fail_closed() {
    let f = Fixture::new("normal");
    let (extension, registry, cancel) = f.start(Some(f.host.clone()));
    ready(&registry).await;
    let working = Arc::new(Mutex::new(false));
    let progress = working.clone();
    let ctx = f.context(
        CancellationToken::new(),
        Arc::new(move |_| *progress.lock().unwrap() = true),
    );
    let interceptor = &registry.interceptors()[0].interceptor;
    // Startup uses real processes; only the deadline assertion uses virtual time.
    tokio::time::pause();
    let start = tokio::time::Instant::now();
    assert!(
        interceptor
            .before(&call("timeout"), ctx.clone())
            .await
            .unwrap_err()
            .0
            .contains("timed out")
    );
    assert!(start.elapsed() >= Duration::from_secs(2));
    assert!(start.elapsed() < Duration::from_millis(2100));
    tokio::time::resume();
    assert!(
        interceptor
            .before(&call("bad-intercept"), ctx.clone())
            .await
            .is_err()
    );
    assert!(
        interceptor
            .after(&call("bad-intercept"), &ToolOutput::text("x"), ctx.clone())
            .await
            .is_err()
    );
    let tool = registry.tools().get("plugin_fixture_echo").unwrap();
    let task = tokio::spawn(async move { tool.run(call("slow"), ctx).await });
    wait("tool progress", || *working.lock().unwrap()).await;
    extension
        .event(&RuntimeEvent::Interrupt, &f.branch())
        .unwrap();
    assert!(task.await.unwrap().is_err());
    wait("tool cancellation", || {
        f.temp.path().join("cancelled").exists()
    })
    .await;
    cancel.cancel();
    f.stopped().await;
}

#[test]
fn interceptor_deadline_excludes_waiting_for_package_freshness() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let f = Fixture::new("normal");
        let (_extension, registry, cancel) = f.start(Some(f.host.clone()));
        ready(&registry).await;
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            let _ = blocked.recv();
        });
        waiting.await.unwrap();
        let interceptor = &registry.interceptors()[0].interceptor;
        let call = call("deny");
        let mut request = Box::pin(
            interceptor.before(&call, f.context(CancellationToken::new(), Arc::new(|_| {}))),
        );
        tokio::select! {
            result = &mut request => panic!("deadline included freshness: {result:?}"),
            _ = tokio::time::sleep(Duration::from_millis(2100)) => {},
        }
        drop(release);
        worker.await.unwrap();
        assert_eq!(
            request.await.unwrap(),
            BeforeToolCall::Deny("Denied by test plugin".into())
        );
        cancel.cancel();
        f.stopped().await;
    });
}

#[test]
fn waiting_for_package_freshness_is_cancellable() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(1)
        .build()
        .unwrap();
    runtime.block_on(async {
        let f = Fixture::new("normal");
        let (_extension, registry, cancel) = f.start(Some(f.host.clone()));
        ready(&registry).await;
        let (release, blocked) = std::sync::mpsc::channel::<()>();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let worker = tokio::task::spawn_blocking(move || {
            started.send(()).unwrap();
            let _ = blocked.recv();
        });
        waiting.await.unwrap();
        let request_cancel = CancellationToken::new();
        let stop = request_cancel.clone();
        tokio::spawn(async move {
            tokio::task::yield_now().await;
            stop.cancel();
        });
        let result = registry.interceptors()[0]
            .interceptor
            .before(&call("deny"), f.context(request_cancel, Arc::new(|_| {})))
            .await;
        assert_eq!(result.unwrap_err().0, "Operation aborted");
        drop(release);
        worker.await.unwrap();
        // A cancelled hash must not revoke an unchanged package's grant.
        assert_eq!(
            registry.interceptors()[0]
                .interceptor
                .before(
                    &call("deny"),
                    f.context(CancellationToken::new(), Arc::new(|_| {}))
                )
                .await
                .unwrap(),
            BeforeToolCall::Deny("Denied by test plugin".into())
        );
        cancel.cancel();
        f.stopped().await;
    });
}

#[tokio::test]
async fn metadata_changes_revoke_content_grants() {
    for change in ["mtime", "size", "inode"] {
        let f = Fixture::new("normal");
        let path = f.plugin.root.join("payload");
        fs::write(&path, "original").unwrap();
        let modified = fs::metadata(&path).unwrap().modified().unwrap();
        let (_extension, registry, cancel) = f.start(Some(f.host.clone()));
        ready(&registry).await;
        match change {
            "mtime" => {
                fs::write(&path, "modified").unwrap();
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(modified + Duration::from_nanos(1_000_000))
                    .unwrap();
            }
            "size" => {
                fs::write(&path, "different size").unwrap();
                fs::File::options()
                    .write(true)
                    .open(&path)
                    .unwrap()
                    .set_modified(modified)
                    .unwrap();
            }
            "inode" => {
                let directory_modified = fs::metadata(&f.plugin.root).unwrap().modified().unwrap();
                let replacement = f.temp.path().join("replacement");
                fs::write(&replacement, "modified").unwrap();
                fs::File::options()
                    .write(true)
                    .open(&replacement)
                    .unwrap()
                    .set_modified(modified)
                    .unwrap();
                fs::rename(replacement, &path).unwrap();
                fs::File::open(&f.plugin.root)
                    .unwrap()
                    .set_modified(directory_modified)
                    .unwrap();
            }
            _ => unreachable!(),
        }
        let tool = registry.tools().get("plugin_fixture_echo").unwrap();
        let error = tool
            .run(
                call("echo"),
                f.context(CancellationToken::new(), Arc::new(|_| {})),
            )
            .await
            .unwrap_err();
        assert!(error.0.contains("revoked"), "{change}: {error}");
        cancel.cancel();
        f.stopped().await;
    }
}

#[tokio::test]
async fn grants_are_content_pinned_and_headless_never_prompts() {
    let f = Fixture::new("normal");
    let (extension, registry, cancel) = f.start(None);
    wait("authorization status", || {
        !extension
            .event(&RuntimeEvent::Tick, &f.branch())
            .unwrap()
            .events
            .is_empty()
    })
    .await;
    assert!(registry.tools().specs().is_empty());
    assert!(f.pids().is_empty());
    cancel.cancel();
    let (_, registry, cancel) = f.start(Some(f.host.clone()));
    ready(&registry).await;
    cancel.cancel();
    f.stopped().await;
    let (_, registry, cancel) = f.start(None);
    ready(&registry).await;
    let tool = registry.tools().get("plugin_fixture_echo").unwrap();
    fs::write(f.plugin.root.join("changed.txt"), "changed").unwrap();
    assert!(
        tool.run(
            call("echo"),
            f.context(CancellationToken::new(), Arc::new(|_| {}))
        )
        .await
        .unwrap_err()
        .0
        .contains("revoked")
    );
    cancel.cancel();
    f.stopped().await;
    let (extension, registry, cancel) = f.start(None);
    wait("authorization status", || {
        !extension
            .event(&RuntimeEvent::Tick, &f.branch())
            .unwrap()
            .events
            .is_empty()
    })
    .await;
    assert!(registry.tools().specs().is_empty());
    cancel.cancel();
    let (_, registry, cancel) = f.start(Some(f.host.clone()));
    ready(&registry).await;
    assert_eq!(f.host.prompts.lock().unwrap().len(), 2);
    cancel.cancel();
    f.stopped().await;
}

#[tokio::test]
async fn decline_and_untrusted_projects_never_spawn() {
    let f = Fixture::new("normal");
    let host = Arc::new(Host {
        decline: true,
        ..Default::default()
    });
    let (extension, _, cancel) = f.start(Some(host.clone()));
    wait("authorization decline", || {
        extension
            .event(&RuntimeEvent::Tick, &f.branch())
            .unwrap()
            .events
            .iter()
            .any(|(_, data)| data["text"] == "disabled: capabilities declined")
    })
    .await;
    assert_eq!(host.prompts.lock().unwrap().len(), 1);
    assert!(f.pids().is_empty());
    assert!(
        Extension::start(
            &f.plugin,
            &f.temp.path().join("home"),
            false,
            Some(host),
            cancel.clone()
        )
        .is_none()
    );
    cancel.cancel();
}

#[tokio::test]
async fn crash_restarts_with_backoff_and_cleans_every_process_group() {
    let f = Fixture::new("crash-once");
    let (_, registry, cancel) = f.start(Some(f.host.clone()));
    ready(&registry).await;
    assert_eq!(f.pids().len(), 4);
    let tool = registry.tools().get("plugin_fixture_echo").unwrap();
    assert!(
        tool.run(
            call("crash"),
            f.context(CancellationToken::new(), Arc::new(|_| {}))
        )
        .await
        .is_err()
    );
    wait("third process start", || f.pids().len() == 6).await;
    ready(&registry).await;
    cancel.cancel();
    f.stopped().await;
}

#[tokio::test]
async fn version_mismatch_and_restart_exhaustion_remain_visible_and_closed() {
    for mode in ["version", "crash"] {
        let f = Fixture::new(mode);
        let (extension, registry, cancel) = f.start(Some(f.host.clone()));
        wait("third process start", || f.pids().len() == 6).await;
        wait("restart exhaustion", || {
            extension
                .event(&RuntimeEvent::Tick, &f.branch())
                .unwrap()
                .events
                .iter()
                .any(|(_, data)| {
                    data["text"]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("failed:")
                })
        })
        .await;
        assert!(registry.tools().specs().is_empty());
        assert!(
            registry.interceptors()[0]
                .interceptor
                .before(
                    &call("echo"),
                    f.context(CancellationToken::new(), Arc::new(|_| {}))
                )
                .await
                .is_err()
        );
        cancel.cancel();
        f.stopped().await;
    }
}

#[tokio::test]
async fn dropping_the_registry_cleans_processes_without_explicit_shutdown() {
    let f = Fixture::new("normal");
    let (extension, registry, _cancel) = f.start(Some(f.host.clone()));
    ready(&registry).await;
    drop(extension);
    drop(registry);
    f.stopped().await;
}

#[test]
fn invalid_panel_trees_are_rejected() {
    let mut node = PanelNode::Text {
        text: "deep".into(),
    };
    for _ in 0..18 {
        node = PanelNode::List {
            children: vec![node],
        };
    }
    assert!(
        tiechui_module_plugins::validate_panel(&PluginPanel {
            title: "x".into(),
            nodes: vec![node]
        })
        .is_err()
    );
    assert!(
        tiechui_module_plugins::validate_panel(&PluginPanel {
            title: "x".into(),
            nodes: vec![PanelNode::Progress {
                label: "bad".into(),
                value: 2,
                total: 1
            }]
        })
        .is_err()
    );
}

#[tokio::test]
async fn undeclared_capabilities_and_unknown_methods_are_rejected_and_headless_ui_cancels() {
    let mut f = Fixture::new("normal");
    f.plugin
        .extension
        .as_mut()
        .unwrap()
        .capabilities
        .retain(|c| *c != PluginCapability::Select);
    let (_, registry, cancel) = f.start(Some(f.host.clone()));
    ready(&registry).await;
    for argument in ["select", "unknown"] {
        registry.commands()[0]
            .run(argument, CancellationToken::new())
            .await
            .unwrap();
    }
    let replies = fs::read_to_string(f.temp.path().join("responses")).unwrap();
    assert!(replies.contains("-32001"));
    assert!(replies.contains("-32601"));
    assert_eq!(f.host.prompts.lock().unwrap().len(), 1);
    cancel.cancel();
    f.stopped().await;
    let (_, registry, cancel) = f.start(None);
    ready(&registry).await;
    registry.commands()[0]
        .run("input", CancellationToken::new())
        .await
        .unwrap();
    assert!(
        fs::read_to_string(f.temp.path().join("responses"))
            .unwrap()
            .contains("\"kind\":\"cancel\"")
    );
    cancel.cancel();
    f.stopped().await;
}
