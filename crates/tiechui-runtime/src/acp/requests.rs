use serde_json::{Map, Value, json};
use tiechui_protocol::{ApprovalDecision, ApprovalRequest, FilePreview, Question, QuestionOption};

use super::dialect::{AcpDialect, CustomApproval, ToolState};
use super::items::{collect_diffs, normalized_call};
use super::wire::{PermissionOptionWire, PermissionRequestWire};

const OTHER_SUFFIXES: [&str; 3] = ["__other", "_other", "_custom"];
const RECOMMENDED_TAG: &[u8] = b"(recommended)";

pub enum Responder {
    Permission(Vec<PermissionOptionWire>),
    Elicitation(Value),
    Custom(Box<dyn FnOnce(&ApprovalDecision) -> Value + Send>),
}

impl Responder {
    pub fn into_response(self, decision: &ApprovalDecision) -> Value {
        match self {
            Self::Permission(options) => permission_response(&options, decision),
            Self::Elicitation(schema) => elicitation_response(&schema, decision),
            Self::Custom(respond) => respond(decision),
        }
    }
}

pub struct PendingApproval {
    pub rpc_id: Value,
    pub responder: Responder,
}

pub fn build_permission_approval(
    params: &Value,
    existing: Option<&ToolState>,
    dialect: &dyn AcpDialect,
) -> Option<(ToolState, ApprovalRequest, Responder)> {
    let parsed: PermissionRequestWire = serde_json::from_value(params.clone()).ok()?;
    if parsed.options.is_empty() {
        return None;
    }
    let mut state = existing.cloned().unwrap_or_default();
    state.merge_from(&parsed.tool_call);
    if state.id.is_empty() {
        return None;
    }
    dialect.normalize_tool(&mut state);
    let (tool, arguments) = normalized_call(&state);
    let has_session_grant = parsed.options.iter().any(|opt| opt.kind == "allow_always");
    let (surface, values, preview) = permission_subject(&tool, &arguments, &state);
    let session_patterns = if has_session_grant {
        values.clone()
    } else {
        Vec::new()
    };
    let request = ApprovalRequest::Permission {
        actor: None,
        call_id: state.id.clone(),
        tool,
        arguments,
        surface,
        values,
        matched_pattern: None,
        origin: dialect.provider_id().into(),
        session_patterns,
        preview,
    };
    Some((state, request, Responder::Permission(parsed.options)))
}

fn permission_subject(
    tool: &str,
    arguments: &Value,
    state: &ToolState,
) -> (String, Vec<String>, Option<Box<FilePreview>>) {
    if tool == "bash" {
        let cmd = arguments["command"]
            .as_str()
            .or(state.title.as_deref())
            .unwrap_or_default()
            .to_owned();
        return ("command".into(), vec![cmd], None);
    }
    if matches!(tool, "write" | "edit") {
        let path = arguments["path"].as_str().unwrap_or_default().to_owned();
        let preview = collect_diffs(&state.content).first().map(|diff| {
            Box::new(FilePreview {
                before: diff.old_text.unwrap_or_default().to_owned(),
                after: diff.new_text.to_owned(),
            })
        });
        return ("path".into(), vec![path], preview);
    }
    let label = state.title.clone().unwrap_or_else(|| tool.to_owned());
    ("tool".into(), vec![label], None)
}

fn pick_option<'a>(options: &'a [PermissionOptionWire], kinds: &[&str]) -> Option<&'a str> {
    kinds.iter().find_map(|wanted| {
        options
            .iter()
            .find(|opt| opt.kind == *wanted && !opt.option_id.is_empty())
            .map(|opt| opt.option_id.as_str())
    })
}

pub fn permission_response(options: &[PermissionOptionWire], decision: &ApprovalDecision) -> Value {
    let chosen = match decision {
        ApprovalDecision::AllowOnce | ApprovalDecision::Accept { .. } => {
            pick_option(options, &["allow_once", "allow_always"])
        }
        ApprovalDecision::AllowSession { .. } => {
            pick_option(options, &["allow_always", "allow_once"])
        }
        ApprovalDecision::Deny { .. } | ApprovalDecision::Decline => {
            pick_option(options, &["reject_once", "reject_always"])
        }
        ApprovalDecision::Cancel => None,
    };
    match chosen {
        Some(option_id) => json!({ "outcome": { "outcome": "selected", "optionId": option_id } }),
        None => json!({ "outcome": { "outcome": "cancelled" } }),
    }
}

pub fn build_elicitation_approval(
    params: &Value,
    default_server: &str,
) -> Option<(ApprovalRequest, Responder)> {
    if params["mode"].as_str() == Some("url") {
        return None;
    }
    let raw_schema = params.get("requestedSchema")?.clone();
    raw_schema.get("properties")?.as_object()?;
    let mut normalized = params.clone();
    if let Some(props) = normalized["requestedSchema"]["properties"].as_object_mut() {
        for prop in props.values_mut() {
            normalize_one_of_for_form(prop);
        }
    }
    let server = params["serverName"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(default_server)
        .to_owned();
    let request = ApprovalRequest::Elicitation {
        server,
        request: normalized,
    };
    Some((request, Responder::Elicitation(raw_schema)))
}

fn normalize_one_of_for_form(prop: &mut Value) {
    if prop.get("enum").and_then(Value::as_array).is_some() {
        return;
    }
    let Some(one_of) = prop.get("oneOf").and_then(Value::as_array) else {
        return;
    };
    let values: Vec<Value> = one_of
        .iter()
        .filter_map(|entry| entry["const"].as_str().map(|s| json!(s)))
        .collect();
    if !values.is_empty()
        && let Some(obj) = prop.as_object_mut()
    {
        obj.insert("enum".into(), Value::Array(values));
    }
}

pub fn elicitation_response(schema: &Value, decision: &ApprovalDecision) -> Value {
    match decision {
        ApprovalDecision::Accept { content } => {
            let coerced = coerce_elicitation_content(schema, content.as_ref());
            json!({ "action": "accept", "content": coerced })
        }
        ApprovalDecision::AllowOnce | ApprovalDecision::AllowSession { .. } => {
            json!({ "action": "accept", "content": {} })
        }
        ApprovalDecision::Deny { .. } | ApprovalDecision::Decline => json!({ "action": "decline" }),
        ApprovalDecision::Cancel => json!({ "action": "cancel" }),
    }
}

fn coerce_elicitation_content(schema: &Value, content: Option<&Value>) -> Value {
    let Some(Value::Object(submitted)) = content else {
        return json!({});
    };
    let props = schema["properties"].as_object();
    let mut out = Map::new();
    for (key, val) in submitted {
        let coerced = match props.and_then(|p| p.get(key)) {
            Some(prop) => coerce_scalar(prop, val),
            None => val.clone(),
        };
        out.insert(key.clone(), coerced);
    }
    Value::Object(out)
}

fn coerce_scalar(prop: &Value, val: &Value) -> Value {
    match prop["type"].as_str() {
        Some("integer") => val
            .as_i64()
            .or_else(|| val.as_str()?.trim().parse().ok())
            .map_or_else(|| val.clone(), |n| json!(n)),
        Some("number") => val
            .as_f64()
            .or_else(|| val.as_str()?.trim().parse().ok())
            .map_or_else(|| val.clone(), |n| json!(n)),
        Some("boolean") => val
            .as_bool()
            .or_else(|| val.as_str()?.trim().parse().ok())
            .map_or_else(|| val.clone(), |b| json!(b)),
        _ => resolve_one_of_const(prop, val),
    }
}

fn resolve_one_of_const(prop: &Value, val: &Value) -> Value {
    let Some(answer) = val.as_str() else {
        return val.clone();
    };
    let Some(one_of) = prop["oneOf"].as_array() else {
        return val.clone();
    };
    one_of
        .iter()
        .find(|item| {
            item["const"].as_str() == Some(answer) || item["title"].as_str() == Some(answer)
        })
        .and_then(|item| item["const"].as_str())
        .map_or_else(|| val.clone(), |c| json!(c))
}

fn strip_recommended(label: &str) -> String {
    let mut current = label.trim().to_owned();
    while let Some(pos) = find_recommended_tag(&current) {
        let end = pos + RECOMMENDED_TAG.len();
        let before = current[..pos].trim_end();
        let after = current[end..].trim_start();
        current = join_trimmed_parts(before, after);
    }
    current
}

fn find_recommended_tag(text: &str) -> Option<usize> {
    text.as_bytes()
        .windows(RECOMMENDED_TAG.len())
        .position(|window| window.eq_ignore_ascii_case(RECOMMENDED_TAG))
}

fn join_trimmed_parts(before: &str, after: &str) -> String {
    match (before.is_empty(), after.is_empty()) {
        (true, _) => after.to_owned(),
        (false, true) => before.to_owned(),
        (false, false) => format!("{before} {after}"),
    }
}

fn extract_question_answers(decision: &ApprovalDecision, question_id: &str) -> Option<Vec<String>> {
    let ApprovalDecision::Accept {
        content: Some(content),
    } = decision
    else {
        return None;
    };
    let entry = content.get("answers")?.get(question_id)?;
    let list = entry
        .get("answers")
        .and_then(Value::as_array)
        .or_else(|| entry.as_array())?;
    let answers: Vec<String> = list
        .iter()
        .filter_map(|v| v.as_str().map(str::to_owned))
        .collect();
    (!answers.is_empty()).then_some(answers)
}

/// Converts an `interaction_*` permission request into `ApprovalRequest::Questions`.
pub fn interaction_permission_as_questions(params: &Value) -> Option<CustomApproval> {
    let parsed: PermissionRequestWire = serde_json::from_value(params.clone()).ok()?;
    let call_id = parsed.tool_call["toolCallId"].as_str()?.to_owned();
    if !call_id.starts_with("interaction_") || parsed.options.is_empty() {
        return None;
    }
    let prompt_text = parsed.tool_call["title"]
        .as_str()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or("Choose an option.")
        .to_owned();
    let q_options: Vec<QuestionOption> = parsed
        .options
        .iter()
        .map(|opt| {
            let label = if opt.name.trim().is_empty() {
                opt.option_id.clone()
            } else {
                opt.name.clone()
            };
            QuestionOption {
                description: String::new(),
                label,
            }
        })
        .collect();
    let qid = call_id.clone();
    let offered = parsed.options;
    Some(CustomApproval {
        request: ApprovalRequest::Questions {
            questions: vec![Question {
                id: call_id,
                header: "Question".into(),
                question: prompt_text,
                options: Some(q_options),
            }],
        },
        respond: Box::new(move |decision| {
            match extract_question_answers(decision, &qid)
                .and_then(|ans| match_interaction_option(&offered, &ans[0]))
            {
                Some(option_id) => {
                    json!({ "outcome": { "outcome": "selected", "optionId": option_id } })
                }
                None => json!({ "outcome": { "outcome": "cancelled" } }),
            }
        }),
    })
}

fn match_interaction_option<'a>(
    options: &'a [PermissionOptionWire],
    chosen: &str,
) -> Option<&'a str> {
    let normalized = strip_recommended(chosen);
    options
        .iter()
        .find(|opt| {
            opt.option_id == chosen
                || opt.name == chosen
                || strip_recommended(&opt.name) == normalized
        })
        .map(|opt| opt.option_id.as_str())
}

struct FormQuestionField {
    key: String,
    other_key: Option<String>,
    prop: Value,
    choices: Vec<(String, String)>,
}

/// Converts a `mode: "form"` `elicitation/create` request into `ApprovalRequest::Questions`,
/// folding optional `<key>__other` write-in fields onto the same question card.
pub fn form_elicitation_as_questions(params: &Value) -> Option<CustomApproval> {
    if params["mode"].as_str().is_some_and(|m| m != "form") {
        return None;
    }
    let props = params
        .get("requestedSchema")?
        .get("properties")?
        .as_object()?;
    if props.is_empty() {
        return None;
    }
    let message = params["message"].as_str().unwrap_or_default();
    let fields = build_form_question_fields(props);
    if fields.is_empty() {
        return None;
    }
    let questions = fields
        .iter()
        .enumerate()
        .map(|(idx, field)| to_protocol_question(idx, field, message))
        .collect();
    Some(CustomApproval {
        request: ApprovalRequest::Questions { questions },
        respond: Box::new(move |decision| answer_form_questions(&fields, decision)),
    })
}

fn build_form_question_fields(props: &Map<String, Value>) -> Vec<FormQuestionField> {
    let mut companions = Map::new();
    for (key, prop) in props {
        if field_choices(prop).is_empty() {
            continue;
        }
        if let Some(comp) = find_companion_key(props, key) {
            companions.insert(key.clone(), Value::String(comp));
        }
    }
    props
        .iter()
        .filter(|(key, _)| {
            !companions
                .values()
                .any(|v| v.as_str() == Some(key.as_str()))
        })
        .map(|(key, prop)| FormQuestionField {
            key: key.clone(),
            other_key: companions
                .get(key)
                .and_then(Value::as_str)
                .map(str::to_owned),
            prop: prop.clone(),
            choices: field_choices(prop),
        })
        .collect()
}

fn find_companion_key(props: &Map<String, Value>, key: &str) -> Option<String> {
    OTHER_SUFFIXES.iter().find_map(|suffix| {
        let candidate = format!("{key}{suffix}");
        let other = props.get(&candidate)?;
        (other["type"] == "string" && field_choices(other).is_empty()).then_some(candidate)
    })
}

fn field_choices(prop: &Value) -> Vec<(String, String)> {
    if prop["type"] == "boolean" {
        return vec![("true".into(), "Yes".into()), ("false".into(), "No".into())];
    }
    if prop["type"] == "array" {
        return array_item_choices(&prop["items"]);
    }
    enum_or_one_of_choices(prop, "oneOf")
}

fn array_item_choices(items: &Value) -> Vec<(String, String)> {
    let choices = enum_or_one_of_choices(items, "anyOf");
    if choices.is_empty() {
        enum_or_one_of_choices(items, "oneOf")
    } else {
        choices
    }
}

fn enum_or_one_of_choices(prop: &Value, key: &str) -> Vec<(String, String)> {
    if let Some(variants) = prop[key].as_array() {
        return variants
            .iter()
            .filter_map(|item| {
                let val = item["const"].as_str()?.to_owned();
                let label = item["title"].as_str().unwrap_or(&val).to_owned();
                Some((val, label))
            })
            .collect();
    }
    prop["enum"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(|s| (s.to_owned(), s.to_owned())))
        .collect()
}

fn to_protocol_question(idx: usize, field: &FormQuestionField, message: &str) -> Question {
    let title = field.prop["title"].as_str().unwrap_or_default();
    let desc = field.prop["description"].as_str().unwrap_or_default();
    let question_text = match (!title.is_empty(), !desc.is_empty()) {
        (true, true) => format!("{title}\n\n{desc}"),
        (true, false) => title.to_owned(),
        (false, true) => desc.to_owned(),
        (false, false) if !message.is_empty() => message.to_owned(),
        _ => field.key.clone(),
    };
    let options = (!field.choices.is_empty()).then(|| {
        field
            .choices
            .iter()
            .map(|(_, label)| QuestionOption {
                label: label.clone(),
                description: String::new(),
            })
            .collect()
    });
    Question {
        id: format!("q{}", idx + 1),
        header: if title.is_empty() {
            field.key.clone()
        } else {
            title.to_owned()
        },
        question: question_text,
        options,
    }
}

fn answer_form_questions(fields: &[FormQuestionField], decision: &ApprovalDecision) -> Value {
    match decision {
        ApprovalDecision::Accept { .. } => {
            let mut out = Map::new();
            for (idx, field) in fields.iter().enumerate() {
                let qid = format!("q{}", idx + 1);
                if let Some(ans) = extract_question_answers(decision, &qid) {
                    apply_question_answers(&mut out, field, &ans);
                }
            }
            json!({ "action": "accept", "content": Value::Object(out) })
        }
        ApprovalDecision::Deny { .. } | ApprovalDecision::Decline => json!({ "action": "decline" }),
        ApprovalDecision::Cancel
        | ApprovalDecision::AllowOnce
        | ApprovalDecision::AllowSession { .. } => json!({ "action": "cancel" }),
    }
}

fn apply_question_answers(
    out: &mut Map<String, Value>,
    field: &FormQuestionField,
    answers: &[String],
) {
    if field.prop["type"] == "array" {
        let mut selected = Vec::new();
        for ans in answers {
            match match_field_choice(&field.choices, ans) {
                Some(val) => selected.push(json!(val)),
                None if field.other_key.is_some() => {
                    if let Some(other) = &field.other_key {
                        out.insert(other.clone(), json!(ans));
                    }
                }
                None => selected.push(json!(ans)),
            }
        }
        if !selected.is_empty() {
            out.insert(field.key.clone(), Value::Array(selected));
        }
        return;
    }
    apply_question_field(out, field, &answers[0]);
}

fn match_field_choice<'a>(choices: &'a [(String, String)], chosen: &str) -> Option<&'a str> {
    let normalized = strip_recommended(chosen);
    choices
        .iter()
        .find(|(val, label)| {
            val == chosen || label == chosen || strip_recommended(label) == normalized
        })
        .map(|(val, _)| val.as_str())
}

fn apply_question_field(out: &mut Map<String, Value>, field: &FormQuestionField, chosen: &str) {
    match (match_field_choice(&field.choices, chosen), &field.other_key) {
        (Some(val), _) => {
            out.insert(field.key.clone(), coerce_scalar(&field.prop, &json!(val)));
        }
        (None, Some(other_key)) => {
            out.insert(other_key.clone(), json!(chosen));
        }
        (None, None) => {
            out.insert(
                field.key.clone(),
                coerce_scalar(&field.prop, &json!(chosen)),
            );
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn strip_recommended_removes_tag_anywhere_case_insensitively_and_collapses_whitespace() {
        assert_eq!(strip_recommended("(Recommended) Rebase"), "Rebase");
        assert_eq!(strip_recommended("Rebase (recommended)"), "Rebase");
        assert_eq!(
            strip_recommended("Option A   (RECOMMENDED)   - fast"),
            "Option A - fast"
        );
        assert_eq!(
            strip_recommended("中文选项 (rEcOmMeNdEd) 说明"),
            "中文选项 说明"
        );
        assert_eq!(strip_recommended("  (Recommended)  "), "");
        assert_eq!(strip_recommended("Plain choice"), "Plain choice");
    }
}
