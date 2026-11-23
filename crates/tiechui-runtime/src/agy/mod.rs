//! Antigravity (`agy acp`) external runtime driver and ACP dialect.

mod errors;
mod tools;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;
use tiechui_protocol::{CommandInfo, ModelInfo, RuntimeCapabilities, SteeringMode, ThinkingLevel};
use tokio::process::Command;

use crate::acp::{
    AcpBackend, AcpConfig, AcpDialect, AcpLaunchSpec, AcpSubagent, AcpTimeouts, CustomApproval,
    InitializeInfo, TextDisposition, ToolState, form_elicitation_as_questions,
    interaction_permission_as_questions,
};
use crate::backend::{Backend, StartParams};
use crate::process::{configure_env, overridable};
use crate::registry::{
    Driver, Launch, PROBE_TIMEOUT, Probe, at_least, parse_version, resolve_in_path, run_bounded_cli,
};
use tools::PathRoot;

pub const MIN_VERSION: &str = "1.5.2";

const MODELS_TIMEOUT: Duration = Duration::from_secs(10);
const HANDSHAKE_TIMEOUT_ENV: &str = "CHUI_AGY_HANDSHAKE_TIMEOUT_MS";
const WAKEUP_IDLE_ENV: &str = "CHUI_AGY_WAKEUP_IDLE_MS";

fn agy_env() -> (Vec<(String, String)>, Vec<String>) {
    (
        vec![("AGY_CLI_DISABLE_AUTO_UPDATE".into(), "true".into())],
        vec!["GEMINI_API_KEY".into(), "GOOGLE_API_KEY".into()],
    )
}

fn is_agy_stripped_env(name: &str) -> bool {
    name.eq_ignore_ascii_case("GEMINI_API_KEY") || name.eq_ignore_ascii_case("GOOGLE_API_KEY")
}

fn version_too_old_reason(version: &str) -> String {
    format!("agy {version} 版本过低，需要 {MIN_VERSION} 及以上")
}

fn current_timeouts() -> AcpTimeouts {
    let defaults = AcpTimeouts::default();
    let handshake = overridable(defaults.handshake, HANDSHAKE_TIMEOUT_ENV);
    AcpTimeouts {
        handshake,
        session: handshake,
        request: defaults.request,
        wakeup_idle: overridable(defaults.wakeup_idle, WAKEUP_IDLE_ENV),
    }
}

pub struct AgyDriver;

#[async_trait]
impl Driver for AgyDriver {
    fn id(&self) -> &'static str {
        "agy"
    }

    fn label(&self) -> &'static str {
        "Antigravity"
    }

    fn capabilities(&self) -> RuntimeCapabilities {
        RuntimeCapabilities {
            steering: SteeringMode::Queue,
            commands: true,
            permissions: true,
            model: true,
            ..RuntimeCapabilities::minimal()
        }
    }

    fn gated_ops(&self) -> &'static [&'static str] {
        &["run_command", "command_input", "permissions", "set_model"]
    }

    async fn probe(&self, launch: &Launch) -> Probe {
        probe_agy(launch).await
    }

    fn backend(&self, launch: &Launch) -> Result<Box<dyn Backend>, String> {
        let config = AcpConfig {
            launch: launch.clone(),
            dialect: Arc::new(AgyDialect::default()),
            timeouts: current_timeouts(),
        };
        Ok(Box::new(AcpBackend::new(config)))
    }
}

async fn probe_agy(launch: &Launch) -> Probe {
    let Some(path) = resolve_in_path("agy", launch.path.as_deref()) else {
        return Probe::unavailable("agy is not on PATH");
    };
    let text = match run_agy_cli(&path, &["--version"], launch, PROBE_TIMEOUT).await {
        Ok(text) => text,
        Err(reason) => return Probe::found_at(path, reason),
    };
    let Some(version) = parse_version(&text) else {
        return Probe::found_at(path, "agy --version printed no version".into());
    };
    if !at_least(&version, MIN_VERSION) {
        return Probe {
            available: false,
            version: Some(version.clone()),
            path: Some(path),
            reason: Some(version_too_old_reason(&version)),
        };
    }
    Probe {
        available: true,
        version: Some(version),
        path: Some(path),
        reason: None,
    }
}

async fn run_agy_cli(
    binary: &Path,
    args: &[&str],
    launch: &Launch,
    timeout: Duration,
) -> Result<String, String> {
    let (env_set, _) = agy_env();
    let mut command = Command::new(binary);
    command.args(args).current_dir(std::env::temp_dir());
    configure_env(
        &mut command,
        launch.path.as_ref(),
        is_agy_stripped_env,
        &env_set,
    );
    let label = format!("agy {}", args.join(" "));
    run_bounded_cli(command, "agy", &label, timeout).await
}

pub fn parse_models_output(text: &str) -> Vec<ModelInfo> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for line in text.lines() {
        let Some(id) = parse_model_line(line) else {
            continue;
        };
        if seen.insert(id.clone()) {
            out.push(ModelInfo {
                provider: "agy".into(),
                id,
                context_window: 0,
                reasoning: false,
                thinking_levels: vec![ThinkingLevel::Off],
            });
        }
    }
    out
}

fn parse_model_line(line: &str) -> Option<String> {
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("Fetching ") || trimmed.starts_with("ID") {
        return None;
    }
    let id = trimmed
        .split_once('\t')
        .map(|(left, _)| left.trim())
        .or_else(|| trimmed.split_whitespace().next())?;
    let valid = !id.is_empty()
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':'));
    valid.then(|| id.to_owned())
}

#[derive(Default)]
pub struct AgyDialect {
    root: Mutex<Option<PathRoot>>,
}

#[async_trait]
impl AcpDialect for AgyDialect {
    fn provider_id(&self) -> &'static str {
        "agy"
    }

    fn launch(&self, _launch: &Launch, params: &StartParams) -> Result<AcpLaunchSpec, String> {
        *self.root.lock().unwrap_or_else(|e| e.into_inner()) = Some(PathRoot::new(&params.cwd));
        let mut args = vec!["acp".to_owned()];
        if params.mode.as_deref() == Some("yolo") {
            args.push("--dangerously-skip-permissions".to_owned());
        }
        let (env_set, env_remove) = agy_env();
        Ok(AcpLaunchSpec {
            program: "agy".into(),
            args,
            env_set,
            env_remove,
        })
    }

    async fn fallback_models(&self, launch: &Launch) -> Vec<ModelInfo> {
        let Some(path) = resolve_in_path("agy", launch.path.as_deref()) else {
            return Vec::new();
        };
        match run_agy_cli(&path, &["models"], launch, MODELS_TIMEOUT).await {
            Ok(output) => parse_models_output(&output),
            Err(_) => Vec::new(),
        }
    }

    fn check_initialize(&self, info: &InitializeInfo) -> Result<(), String> {
        if let Some(raw_version) = &info.agent_version
            && let Some(version) = parse_version(raw_version)
            && !at_least(&version, MIN_VERSION)
        {
            return Err(version_too_old_reason(&version));
        }
        Ok(())
    }

    fn session_mode(&self, mode: &str, available: &[String]) -> Option<String> {
        let target = match mode {
            "yolo" => return None,
            "auto" => "accept-edits",
            _ => "default",
        };
        if available.is_empty() || available.iter().any(|m| m == target) {
            Some(target.to_owned())
        } else {
            None
        }
    }

    fn mode_requires_restart(&self, current: Option<&str>, next: &str) -> bool {
        (current == Some("yolo")) != (next == "yolo")
    }

    fn filter_command(&self, command: &CommandInfo) -> bool {
        !command
            .name
            .trim()
            .trim_start_matches('/')
            .eq_ignore_ascii_case("logout")
    }

    fn on_text_chunk(&self, thinking: bool, text: &str) -> TextDisposition {
        errors::classify_text_chunk(thinking, text)
    }

    fn normalize_tool(&self, tool: &mut ToolState) {
        let root = self.root.lock().unwrap_or_else(|e| e.into_inner()).clone();
        tools::normalize_tool(tool, root.as_ref());
    }

    fn tool_subagent(&self, tool: &ToolState) -> Option<AcpSubagent> {
        tools::tool_subagent(tool)
    }

    fn intercept_request(&self, method: &str, params: &Value) -> Option<CustomApproval> {
        match method {
            "session/request_permission" => interaction_permission_as_questions(params),
            "elicitation/create" => form_elicitation_as_questions(params),
            _ => None,
        }
    }

    fn format_error(&self, code: i64, message: &str, data: Option<&Value>) -> String {
        errors::format_error(code, message, data)
    }

    fn is_auth_error(&self, code: i64, message: &str, data: Option<&Value>) -> bool {
        errors::is_auth_error(code, message, data)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::path::Path;

    use serde_json::json;

    use super::*;

    #[test]
    fn parses_tab_and_space_delimited_agy_models_output() {
        let sample = "Fetching available models...\ngemini-4-neon-high\tGemini 4 Neon (High)\ngemini-3.8-flash-low\tGemini 3.8 Flash (Low)\ngemini-4-neon-high\tDuplicate\n";
        let models = parse_models_output(sample);
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["gemini-4-neon-high", "gemini-3.8-flash-low"]);
    }

    #[test]
    fn yolo_session_mode_returns_none_and_missing_mode_returns_none() {
        let dialect = AgyDialect::default();
        let modes = vec!["default".to_owned(), "plan".to_owned()];
        assert_eq!(dialect.session_mode("yolo", &modes), None);
        assert_eq!(dialect.session_mode("yolo", &[]), None);
        assert_eq!(dialect.session_mode("auto", &modes), None);
        assert_eq!(
            dialect.session_mode("rules", &modes).as_deref(),
            Some("default")
        );
    }

    #[test]
    fn relativizes_workspace_paths_and_file_uris() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("src/hello.ts");
        let root = PathRoot::new(dir.path());
        assert_eq!(root.relativize(&sub.display().to_string()), "src/hello.ts");
        assert_eq!(root.relativize(&dir.path().display().to_string()), ".");
        assert_eq!(
            root.relativize(&format!("file://{}", sub.display())),
            "src/hello.ts"
        );
        assert_eq!(
            root.relativize(Path::new("/etc/hosts").to_str().unwrap()),
            "/etc/hosts"
        );
    }

    #[test]
    fn classifies_retry_notice_and_cloud_code_errors() {
        assert_eq!(
            errors::classify_text_chunk(false, "⚠ Model produced invalid output.\n"),
            TextDisposition::Ignore
        );
        assert_eq!(
            errors::format_error(
                -32603,
                "Internal error: {\"error\":{\"code\":429,\"message\":\"Quota exceeded.\"}}\n    at Server.run (/x.js:1:2)",
                None,
            ),
            "Quota exceeded."
        );
        assert_eq!(
            errors::format_error(401, "Unauthorized", Some(&json!({}))),
            errors::LOGIN_REQUIRED_MESSAGE
        );
    }

    #[test]
    fn only_agy_status_notices_fail_the_turn() {
        for text in [
            "⚠️ 注意 (code review 前先跑测试)",
            "⚠️ You're not logged in to npm",
            "⚠ 注意 (code review 前先跑测试)",
            "⚠ Not logged in to npm",
        ] {
            assert_eq!(
                errors::classify_text_chunk(false, text),
                TextDisposition::Keep,
                "{text}"
            );
        }
        assert_eq!(
            errors::classify_text_chunk(false, "\n⚠ UNAUTHENTICATED (code 401): token expired\n"),
            TextDisposition::FailTurn(errors::LOGIN_REQUIRED_MESSAGE.into())
        );
        assert_eq!(
            errors::classify_text_chunk(false, "⚠ RESOURCE_EXHAUSTED (code 429): Quota exceeded."),
            TextDisposition::FailTurn("RESOURCE_EXHAUSTED (code 429): Quota exceeded.".into())
        );
    }
}
