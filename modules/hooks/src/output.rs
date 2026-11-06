use serde::Deserialize;
use tiechui_plugin_api::{
    ApprovalVerdict,
    serde_json::{self, Value},
};

use crate::Event;

#[derive(Debug, Default)]
pub struct Outcome {
    pub context: Vec<String>,
    pub block: Option<String>,
    pub stop: Option<String>,
    pub updated_input: Option<Value>,
    pub approval: Option<ApprovalVerdict>,
    pub diagnostics: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Wire {
    #[serde(default = "yes")]
    r#continue: bool,
    stop_reason: Option<String>,
    #[serde(default)]
    suppress_output: bool,
    system_message: Option<String>,
    decision: Option<String>,
    reason: Option<String>,
    hook_specific_output: Option<Value>,
}

fn yes() -> bool {
    true
}
fn nonempty(value: Option<&str>) -> Option<String> {
    value
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_owned)
}

pub(crate) fn parse(event: Event, output: std::process::Output) -> Outcome {
    let mut result = Outcome::default();
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.code() != Some(0) {
        if output.status.code() == Some(2)
            && matches!(
                event,
                Event::PreToolUse
                    | Event::PermissionRequest
                    | Event::UserPromptSubmit
                    | Event::Stop
            )
        {
            if let Some(reason) = nonempty(Some(&stderr)) {
                if event == Event::PermissionRequest {
                    result.approval = Some(ApprovalVerdict::Deny(reason));
                } else {
                    result.block = Some(reason);
                }
            } else {
                result
                    .diagnostics
                    .push("hook exited with code 2 without a reason on stderr".into());
            }
        } else if output.status.code() == Some(2) && event == Event::PostToolUse {
            if let Some(reason) = nonempty(Some(&stderr)) {
                result.context.push(reason);
            }
        } else {
            result.diagnostics.push(format!(
                "hook exited with {:?}: {}",
                output.status.code(),
                stderr.trim()
            ));
        }
        return result;
    }
    if event == Event::SessionEnd || stdout.trim().is_empty() {
        return result;
    }
    let wire = match serde_json::from_str::<Value>(&stdout)
        .ok()
        .filter(Value::is_object)
        .and_then(|v| serde_json::from_value::<Wire>(v).ok())
    {
        Some(wire) => wire,
        None => {
            if stdout.trim_start().starts_with(['{', '[']) {
                result
                    .diagnostics
                    .push("hook returned invalid JSON output".into());
            } else if matches!(event, Event::SessionStart | Event::UserPromptSubmit) {
                result.context.push(stdout.trim().into());
            }
            return result;
        }
    };
    if let Some(message) = &wire.system_message {
        result.diagnostics.push(message.clone());
    }
    if let Err(error) = apply(event, wire, &mut result) {
        result.block = None;
        result.stop = None;
        result.updated_input = None;
        result.approval = None;
        result.context.clear();
        result.diagnostics.push(error.into());
    }
    result
}

fn apply(event: Event, wire: Wire, result: &mut Outcome) -> Result<(), &'static str> {
    let specific = wire.hook_specific_output.as_ref().filter(|v| !v.is_null());
    let fields: &[&str] = match event {
        Event::PreToolUse => &[
            "hookEventName",
            "permissionDecision",
            "permissionDecisionReason",
            "updatedInput",
            "additionalContext",
        ],
        Event::PermissionRequest => &["hookEventName", "decision"],
        Event::PostToolUse => &["hookEventName", "additionalContext", "updatedMCPToolOutput"],
        Event::SessionStart | Event::UserPromptSubmit => &["hookEventName", "additionalContext"],
        _ => &[],
    };
    if let Some(specific) = specific {
        let map = specific.as_object().ok_or("invalid hookSpecificOutput")?;
        if map.keys().any(|key| !fields.contains(&key.as_str()))
            || specific["hookEventName"] != event.wire()
        {
            return Err("unsupported hookSpecificOutput fields or hookEventName");
        }
        for key in fields.iter().filter(|key| {
            !matches!(
                **key,
                "hookEventName" | "decision" | "updatedInput" | "updatedMCPToolOutput"
            )
        }) {
            if let Some(value) = map.get(*key)
                && !value.is_null()
                && !value.is_string()
            {
                return Err("invalid hookSpecificOutput string");
            }
        }
    }
    let empty = Value::Null;
    let specific = specific.unwrap_or(&empty);
    if matches!(event, Event::PreToolUse | Event::PermissionRequest)
        && (!wire.r#continue || wire.stop_reason.is_some() || wire.suppress_output)
    {
        return Err("unsupported universal control field");
    }
    if matches!(
        event,
        Event::SessionStart | Event::PermissionRequest | Event::PreCompact
    ) && (wire.decision.is_some() || wire.reason.is_some())
    {
        return Err("unsupported decision or reason");
    }
    if matches!(event, Event::Stop | Event::PreCompact) && !specific.is_null() {
        return Err("unsupported hookSpecificOutput");
    }
    if event == Event::PostToolUse
        && (wire.suppress_output || !specific["updatedMCPToolOutput"].is_null())
    {
        return Err("unsupported PostToolUse output field");
    }
    match event {
        Event::PreToolUse => {
            let decision = specific["permissionDecision"].as_str();
            let reason = specific["permissionDecisionReason"].as_str();
            let updated = &specific["updatedInput"];
            if decision.is_some() || reason.is_some() || !updated.is_null() {
                match decision {
                    Some("allow") if !updated.is_null() => {
                        result.updated_input = Some(updated.clone())
                    }
                    Some("deny") if updated.is_null() => {
                        result.block =
                            Some(nonempty(reason).ok_or("deny requires permissionDecisionReason")?)
                    }
                    _ => return Err("unsupported PreToolUse permission decision or updatedInput"),
                }
            } else {
                match wire.decision.as_deref() {
                    Some("block") => {
                        result.block =
                            Some(nonempty(wire.reason.as_deref()).ok_or("block requires reason")?)
                    }
                    None if wire.reason.is_none() => {}
                    _ => return Err("unsupported PreToolUse decision"),
                }
            }
        }
        Event::PermissionRequest => {
            if let Some(decision) = specific.get("decision").filter(|v| !v.is_null()) {
                let map = decision.as_object().ok_or("invalid permission decision")?;
                if map.keys().any(|key| {
                    ![
                        "behavior",
                        "message",
                        "updatedInput",
                        "updatedPermissions",
                        "interrupt",
                    ]
                    .contains(&key.as_str())
                }) || !decision["updatedInput"].is_null()
                    || !decision["updatedPermissions"].is_null()
                    || decision.get("interrupt").is_some_and(|v| v != false)
                    || decision
                        .get("message")
                        .is_some_and(|v| !v.is_null() && !v.is_string())
                {
                    return Err("unsupported PermissionRequest decision fields");
                }
                result.approval = Some(match decision["behavior"].as_str() {
                    Some("allow") => ApprovalVerdict::Allow,
                    Some("deny") => ApprovalVerdict::Deny(
                        nonempty(decision["message"].as_str())
                            .unwrap_or_else(|| "PermissionRequest hook denied approval".into()),
                    ),
                    _ => return Err("invalid PermissionRequest behavior"),
                });
            }
        }
        _ => {
            if !wire.r#continue {
                result.stop = Some(
                    wire.stop_reason
                        .unwrap_or_else(|| format!("{} hook stopped execution", event.wire())),
                );
            } else if let Some(decision) = wire.decision {
                if decision != "block"
                    || !matches!(
                        event,
                        Event::Stop | Event::UserPromptSubmit | Event::PostToolUse
                    )
                {
                    return Err("unsupported decision");
                }
                let reason = nonempty(wire.reason.as_deref()).ok_or("block requires reason")?;
                if event == Event::PostToolUse {
                    result.context.push(reason);
                } else {
                    result.block = Some(reason);
                }
            }
        }
    }
    if let Some(context) = specific["additionalContext"].as_str() {
        result.context.push(context.into());
    }
    Ok(())
}
