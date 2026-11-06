#[path = "mod.rs"]
mod support;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async {
        if let Ok(path) = std::env::var("MCP_PID_FILE") {
            let child = std::process::Command::new("/bin/sh")
                .args(["-c", "sleep 300 & wait"])
                .spawn()?;
            std::fs::write(path, format!("{} {}", std::process::id(), child.id()))?;
        }
        let delay = std::env::args()
            .nth(1)
            .and_then(|s| s.parse::<u64>().ok())
            .unwrap_or_default();
        let handler = support::TestServer {
            delay: std::time::Duration::from_millis(delay),
            ..Default::default()
        };
        let service = rmcp::serve_server(handler, rmcp::transport::stdio()).await?;
        service.waiting().await?;
        Ok(())
    })
}
