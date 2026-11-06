//! Offline host fixture implemented with the public SDK.
#![allow(clippy::unwrap_used)]
use std::{io::Write, path::PathBuf, time::Duration};
use tiechui_plugin_api::serde_json::{self, Value, json};
use tiechui_plugin_sdk::{protocol::*, *};

struct Fixture {
    directory: PathBuf,
    mode: String,
}
struct Cancelled {
    directory: PathBuf,
    token: CancellationToken,
}
impl Drop for Cancelled {
    fn drop(&mut self) {
        if self.token.is_cancelled() {
            std::fs::write(self.directory.join("cancelled"), "yes").unwrap();
        }
    }
}
#[async_trait]
impl Plugin for Fixture {
    async fn initialize(&self, _: &PluginInitialize, _: CancellationToken) -> Result<()> {
        if self.mode == "version" {
            return Err(error(-32002, "Incompatible API version"));
        }
        Ok(())
    }
    async fn tool(&self, _: &str, arguments: Value, context: Context) -> Result<PluginOutput> {
        match arguments["action"].as_str().unwrap_or("echo") {
            "notify" => context.notify("Notice", "Plugin notification").await?,
            "crash" => std::process::exit(8),
            "slow" => {
                let _guard = Cancelled {
                    directory: self.directory.clone(),
                    token: context.cancel.clone(),
                };
                loop {
                    context.progress(1.0, "Plugin working")?;
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
            }
            _ => context.progress(1.0, "Plugin progress")?,
        }
        Ok(PluginOutput {
            text: "Plugin tool result".into(),
            is_error: false,
        })
    }
    async fn before(
        &self,
        _: PluginStage,
        call: PluginCall,
        context: Context,
    ) -> Result<PluginVerdict> {
        match call.arguments["action"].as_str() {
            Some("timeout") => {
                context.cancel.cancelled().await;
                Err(error(-32800, "Cancelled"))
            }
            Some("bad-intercept") => Err(error(-32000, "Invalid fixture interception")),
            Some("deny") => Ok(PluginVerdict::Deny {
                reason: "Denied by test plugin".into(),
            }),
            Some("rewrite") => Ok(PluginVerdict::Rewrite {
                arguments: json!({"action":"echo"}),
            }),
            _ => Ok(PluginVerdict::Allow),
        }
    }
    async fn after(
        &self,
        _: PluginStage,
        call: PluginCall,
        _: Value,
        _: Context,
    ) -> Result<Option<PluginOutput>> {
        if call.arguments["action"] == "bad-intercept" {
            return Err(error(-32000, "Invalid fixture interception"));
        }
        Ok(None)
    }
    async fn command(&self, _: &str, arguments: &str, context: Context) -> Result<String> {
        let response = match arguments {
            "notify" => context.notify("Notice", "Plugin notification").await.map(|()| Value::Null),
            "select" => context.select("Choose", "Pick one", vec!["First".into(), "Second".into()]).await.map(|v| json!(v)),
            "confirm" => context.confirm("Confirm", "Continue?").await.map(|v| json!(v)),
            "input" => context.input("Input", "Enter text").await.map(|v| json!(v)),
            "status" => context.status("Fixture status").await.map(|()| Value::Null),
            "unknown" => context.request::<Value>("unknown", json!({})).await,
            _ => context.panel(serde_json::from_value(json!({"title":"Test plugin panel","nodes":[
                {"kind":"text","text":"Declarative text"},
                {"kind":"markdown","text":"**Markdown node**"},
                {"kind":"list","children":[{"kind":"text","text":"List entry"}]},
                {"kind":"progress","label":"Build","value":1,"total":2},
                {"kind":"form","fields":[{"id":"answer","label":"Panel answer","required":true}]}
            ]})).unwrap()).await.map(|v| json!(v)),
        };
        let mut responses = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(self.directory.join("responses"))
            .unwrap();
        writeln!(
            responses,
            "{}",
            match response {
                Ok(v) => json!({"result":v}),
                Err(e) => json!({"error":e}),
            }
        )
        .unwrap();
        Ok("UI completed".into())
    }
}
#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<_> = std::env::args().collect();
    if args.get(1).is_some_and(|arg| arg == "--descendant") {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
        }
    }
    let directory = PathBuf::from(&args[1]);
    let mode = args.get(2).cloned().unwrap_or_else(|| "normal".into());
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .arg("--descendant")
        .spawn()
        .unwrap();
    let mut pids = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(directory.join("pids"))
        .unwrap();
    writeln!(pids, "{} {}", std::process::id(), child.id()).unwrap();
    std::thread::spawn(move || {
        let _ = child.wait();
    });
    if mode == "crash" || (mode == "crash-once" && !directory.join("crashed").exists()) {
        std::fs::write(directory.join("crashed"), "yes").unwrap();
        std::process::exit(7);
    }
    if let Err(error) = stdio(Fixture { directory, mode }, CancellationToken::new()).await {
        eprintln!("{}", error.message);
    }
}
