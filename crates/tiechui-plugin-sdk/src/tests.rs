#![allow(clippy::unwrap_used)]
use crate::*;
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Fixture;
#[async_trait]
impl Plugin for Fixture {
    async fn tool(&self, _: &str, arguments: Value, ctx: Context) -> Result<PluginOutput> {
        ctx.progress(1.0, "Working")?;
        Ok(PluginOutput {
            text: arguments["text"].as_str().unwrap_or_default().into(),
            is_error: false,
        })
    }
    async fn before(&self, _: PluginStage, _: PluginCall, _: Context) -> Result<PluginVerdict> {
        Ok(PluginVerdict::Deny {
            reason: "Blocked".into(),
        })
    }
    async fn after(
        &self,
        _: PluginStage,
        _: PluginCall,
        _: Value,
        _: Context,
    ) -> Result<Option<PluginOutput>> {
        Ok(None)
    }
    async fn command(&self, _: &str, argument: &str, ctx: Context) -> Result<String> {
        match argument {
            "notify" => {
                let (): () = ctx.notify("Title", "Text").await?;
            }
            "select" => {
                ctx.select("Title", "Text", vec!["First".into()]).await?;
            }
            "confirm" => {
                ctx.confirm("Title", "Text").await?;
            }
            "input" => {
                ctx.input("Title", "Text").await?;
            }
            "status" => {
                ctx.status("Ready").await?;
            }
            _ => {
                ctx.panel(PluginPanel {
                    title: "Title".into(),
                    nodes: vec![PanelNode::Text {
                        text: "Text".into(),
                    }],
                })
                .await?;
            }
        }
        Ok("Done".into())
    }
}

#[tokio::test]
async fn recorded_transcript_covers_handlers_helpers_progress_and_cancellation() {
    let (host, plugin) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(plugin);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve(read, write, Fixture, cancel.clone()));
    let (read, mut write) = tokio::io::split(host);
    let mut lines = BufReader::new(read).lines();
    let transcript = include_str!("../../../tests/plugins/transcripts/sdk.jsonl");
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        for line in transcript.lines() {
            let step: Value = serde_json::from_str(line).unwrap();
            if let Some(value) = step.get("send") {
                let bytes = format!("{value}\n").into_bytes();
                for byte in bytes {
                    write.write_all(&[byte]).await.unwrap();
                }
            } else {
                let actual: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                // JSON numbers may be serialized as either integer or float.
                if actual.get("method") == Some(&json!("notifications/progress")) {
                    assert_eq!(actual["params"]["progress"].as_f64(), Some(1.0));
                    let mut expected = step["receive"].clone();
                    expected["params"]["progress"] = json!(1.0);
                    assert_eq!(actual, expected);
                } else {
                    assert_eq!(actual, step["receive"]);
                }
            }
        }
        task.await.unwrap().unwrap();
    })
    .await
    .unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn invalid_and_oversized_frames_close_the_transport() {
    for bytes in [
        b"[]\n".to_vec(), b"{\"jsonrpc\":\"2.0\",\"id\":-1,\"result\":null}\n".to_vec(),
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"x\",\"params\":null}\n".to_vec(),
        b"{\"jsonrpc\":\"2.0\",\"id\":1,\"result\":null,\"error\":{\"code\":1,\"message\":\"x\"}}\n".to_vec(),
        vec![0xff, b'\n'], b"{".to_vec(), vec![b' '; 1024 * 1024 + 1],
    ] {
        assert!(serve(std::io::Cursor::new(bytes), tokio::io::sink(), Fixture, CancellationToken::new()).await.is_err());
    }
}

#[tokio::test]
async fn cancellation_interrupts_a_partial_frame() {
    let (mut host, plugin) = tokio::io::duplex(64);
    let (read, write) = tokio::io::split(plugin);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve(read, write, Fixture, cancel.clone()));
    host.write_all(b"{").await.unwrap();
    cancel.cancel();
    tokio::time::timeout(std::time::Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn initialization_can_be_cancelled_without_blocking_the_reader() {
    struct Slow;
    #[async_trait]
    impl Plugin for Slow {
        async fn initialize(&self, _: &PluginInitialize, _: CancellationToken) -> Result<()> {
            std::future::pending().await
        }
    }
    let (host, plugin) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(plugin);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve(read, write, Slow, cancel.clone()));
    let (read, mut write) = tokio::io::split(host);
    let mut lines = BufReader::new(read).lines();
    write.write_all(b"{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",\"params\":{\"apiVersion\":1,\"capabilities\":[],\"contributions\":{}}}\n{\"jsonrpc\":\"2.0\",\"method\":\"notifications/cancelled\",\"params\":{\"requestId\":1}}\n").await.unwrap();
    let line = tokio::time::timeout(std::time::Duration::from_secs(1), lines.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let response: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(response["error"]["code"], -32800);
    cancel.cancel();
    task.await.unwrap().unwrap();
}

#[tokio::test]
async fn version_and_capability_errors_are_typed() {
    let (host, plugin) = tokio::io::duplex(4096);
    let (read, write) = tokio::io::split(plugin);
    let cancel = CancellationToken::new();
    let task = tokio::spawn(serve(read, write, Fixture, cancel.clone()));
    let (read, mut write) = tokio::io::split(host);
    let mut lines = BufReader::new(read).lines();
    for (id, method, params, code) in [
        (
            1,
            "initialize",
            json!({"apiVersion":2,"capabilities":[],"contributions":{}}),
            Some(-32002),
        ),
        (
            2,
            "initialize",
            json!({"apiVersion":1,"capabilities":[],"contributions":{}}),
            None,
        ),
        (3, "tools/list", json!({}), Some(-32001)),
        (4, "initialize", json!({}), Some(-32600)),
        (5, "shutdown", json!({}), None),
    ] {
        write
            .write_all(
                format!(
                    "{}\n",
                    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        let response: Value =
            serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
        assert_eq!(response["error"]["code"].as_i64(), code);
    }
    task.await.unwrap().unwrap();
}
