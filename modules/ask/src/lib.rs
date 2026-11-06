//! Structured questions, with presentation supplied by the interactive client.

use std::collections::HashSet;
use std::sync::Arc;

use tiechui_plugin_api::{
    CancellationToken, Concurrency, Module, ModuleConfig, Registry, Tool, ToolContext, ToolError,
    ToolOutput, async_trait,
    model::{ToolCall, ToolSpec},
    protocol::Question,
    serde_json::{self, Value, json},
};

#[async_trait]
pub trait QuestionRequester: Send + Sync {
    async fn request(
        &self,
        questions: Vec<Question>,
        cancel: CancellationToken,
    ) -> Result<Option<Value>, ToolError>;
}

pub struct AskModule(pub Option<Arc<dyn QuestionRequester>>);

impl Module for AskModule {
    fn id(&self) -> &'static str {
        "ask"
    }

    fn register(&self, registry: &mut Registry, _: &ModuleConfig) -> Result<(), ToolError> {
        registry.register_tool(Arc::new(Ask {
            requester: self.0.clone(),
            spec: ToolSpec {
                name: "request_user_input".into(),
                description: "Ask the user one to three short, self-contained questions. Prefer two or three mutually exclusive options, recommended choice first. The UI adds None of the above and optional notes. Omit options for free-text answers. Use it for missing information or preferences that affect the task. Do independent work before asking; prefer one concise question. Do not interpret a highlighted option, draft, cancellation or empty answer as the user's choice.".into(),
                input_schema: json!({
                    "type":"object", "additionalProperties":false, "required":["questions"],
                    "properties":{"questions":{"type":"array","minItems":1,"maxItems":3,"items":{
                        "type":"object","additionalProperties":false,"required":["id","header","question"],
                        "properties":{
                            "id":{"type":"string","minLength":1,"maxLength":64,"pattern":"^[a-z][a-z0-9_]*$","description":"Unique snake_case identifier used to map the answer."},
                            "header":{"type":"string","minLength":1,"maxLength":12,"description":"Short label for this question."},
                            "question":{"type":"string","minLength":1,"maxLength":2000,"description":"A clear, self-contained question."},
                            "options":{"type":"array","minItems":2,"maxItems":3,"description":"Two or three mutually exclusive choices. Omit for a free-text question.","items":{
                                "type":"object","additionalProperties":false,"required":["label","description"],
                                "properties":{
                                    "label":{"type":"string","minLength":1,"maxLength":160,"description":"Short user-facing label. Put (Recommended) on the recommended first choice."},
                                    "description":{"type":"string","minLength":1,"maxLength":600,"description":"One sentence explaining the choice's impact or tradeoff."}
                                }
                            }}
                        }
                    }}}
                }),
            },
        }))
    }
}

struct Ask {
    requester: Option<Arc<dyn QuestionRequester>>,
    spec: ToolSpec,
}

fn questions(arguments: &Value) -> Result<Vec<Question>, ToolError> {
    let questions: Vec<Question> = serde_json::from_value(arguments["questions"].clone())
        .map_err(|error| ToolError(error.to_string()))?;
    if !(1..=3).contains(&questions.len()) {
        return Err(ToolError(
            "request_user_input requires one to three questions".into(),
        ));
    }
    let mut ids = HashSet::new();
    for question in &questions {
        let id = question.id.as_bytes();
        if id.is_empty()
            || id.len() > 64
            || !id[0].is_ascii_lowercase()
            || !id
                .iter()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
            || !ids.insert(&question.id)
        {
            return Err(ToolError(
                "Each question needs a unique snake_case id".into(),
            ));
        }
        if question.header.trim().is_empty()
            || question.header.chars().count() > 12
            || question.question.trim().is_empty()
            || question.question.chars().count() > 2000
        {
            return Err(ToolError(
                "Each question needs a short header and non-empty question text".into(),
            ));
        }
        if question.options.as_ref().is_some_and(|options| {
            !(2..=3).contains(&options.len())
                || options.iter().any(|option| {
                    option.label.trim().is_empty()
                        || option.description.trim().is_empty()
                        || option.label.chars().count() > 160
                        || option.description.chars().count() > 600
                })
        }) {
            return Err(ToolError(
                "Use two or three non-empty choices, or omit options for a free-text question"
                    .into(),
            ));
        }
    }
    Ok(questions)
}

#[async_trait]
impl Tool for Ask {
    fn source(&self) -> Option<String> {
        Some("Built-in tools".into())
    }
    fn spec(&self) -> &ToolSpec {
        &self.spec
    }
    fn concurrency(&self, _: &Value) -> Concurrency {
        Concurrency::Exclusive
    }

    async fn run(&self, call: ToolCall, ctx: ToolContext) -> Result<ToolOutput, ToolError> {
        let result = async {
            let questions = questions(&call.arguments)?;
            let requester = self.requester.as_ref().ok_or_else(|| {
                ToolError("request_user_input requires interactive TUI mode.".into())
            })?;
            let response = requester
                .request(questions.clone(), ctx.cancel.clone())
                .await?;
            if ctx.cancel.is_cancelled() || response.is_none() {
                return Err(ToolError(
                    "request_user_input was cancelled before receiving a response.".into(),
                ));
            }
            let response = response.unwrap_or_default();
            let answers = response["answers"]
                .as_object()
                .ok_or_else(|| ToolError("Invalid question response".into()))?;
            if answers.len() != questions.len()
                || questions.iter().any(|question| {
                    answers
                        .get(&question.id)
                        .and_then(|value| value["answers"].as_array())
                        .is_none_or(|values| !values.iter().all(Value::is_string))
                })
            {
                return Err(ToolError("Invalid question response".into()));
            }
            Ok::<_, ToolError>((questions, response))
        }
        .await;
        match result {
            Ok((questions, response)) => Ok(ToolOutput {
                details: Some(
                    json!({"kind":"ask", "questions":questions, "response":response, "status":"submitted"}),
                ),
                structured_content: Some(response.clone()),
                ..ToolOutput::text(response.to_string())
            }),
            Err(error) => {
                let questions = call.arguments["questions"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter(|question| {
                        question["id"].is_string() && question["question"].is_string()
                    })
                    .cloned()
                    .collect::<Vec<_>>();
                let cancelled =
                    ctx.cancel.is_cancelled() || error.0.to_ascii_lowercase().contains("cancel");
                Ok(ToolOutput {
                    is_error: true,
                    details: Some(json!({"kind":"ask", "questions":questions,
                        "response":{"answers":{}}, "status":if cancelled {"cancelled"} else {"error"}, "message":error.0})),
                    ..ToolOutput::text(error.0)
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiechui_plugin_api::{
        protocol::{SessionEpoch, ThreadId, TurnId},
        tools::ExecutionScope,
    };

    fn arguments() -> Value {
        json!({"questions":[{"id":"choice","header":"Choice","question":"Which one?","options":[
            {"label":"A (Recommended)","description":"First"},{"label":"B","description":"Second"}
        ]},{"id":"text","header":"Text","question":"Anything else?"}]})
    }

    fn context(cancel: CancellationToken) -> ToolContext {
        ToolContext {
            invoker: None,
            branch: Default::default(),
            parent_session: None,
            cwd: ".".into(),
            cancel,
            epoch: SessionEpoch(0),
            scope: ExecutionScope {
                thread_id: ThreadId("t".into()),
                turn_id: TurnId("turn".into()),
            },
            updates: Arc::new(|_| {}),
            append_entry: Arc::new(|_| Ok(())),
            origin: Default::default(),
        }
    }

    fn tool(requester: Option<Arc<dyn QuestionRequester>>) -> Arc<dyn Tool> {
        let mut registry = Registry::default();
        AskModule(requester)
            .register(&mut registry, &ModuleConfig::default())
            .unwrap();
        registry.tools().get("request_user_input").unwrap()
    }

    fn call() -> ToolCall {
        ToolCall {
            id: "c".into(),
            name: "request_user_input".into(),
            arguments: arguments(),
            opaque: None,
        }
    }

    #[test]
    fn schema_and_semantic_validation() {
        let tool = tool(None);
        assert_eq!(
            tool.spec().input_schema["properties"]["questions"]["maxItems"],
            3
        );
        assert_eq!(questions(&arguments()).unwrap().len(), 2);
        let mut invalid = arguments();
        invalid["questions"][1]["id"] = json!("choice");
        assert!(questions(&invalid).is_err());
        invalid = arguments();
        invalid["questions"][0]["options"][0]["label"] = json!(" ");
        assert!(questions(&invalid).is_err());
        invalid = arguments();
        invalid["questions"][1]["header"] = json!("十三十四十五十六十七十八十九");
        assert!(questions(&invalid).is_err());
        assert!(questions(&json!({"questions":[]})).is_err());
    }

    struct Answer;
    #[async_trait]
    impl QuestionRequester for Answer {
        async fn request(
            &self,
            questions: Vec<Question>,
            _: CancellationToken,
        ) -> Result<Option<Value>, ToolError> {
            assert_eq!(questions.len(), 2);
            Ok(Some(
                json!({"answers":{"choice":{"answers":["B","user_note: a note"]},"text":{"answers":[]}}}),
            ))
        }
    }

    #[tokio::test]
    async fn headless_errors_and_submitted_record_preserves_empty_answers() {
        let error = tool(None)
            .run(call(), context(CancellationToken::new()))
            .await
            .unwrap();
        assert!(error.is_error);
        let details = error.details.unwrap();
        assert_eq!(details["status"], "error");
        assert_eq!(details["questions"], arguments()["questions"]);
        assert_eq!(
            details["message"],
            "request_user_input requires interactive TUI mode."
        );
        let tool = tool(Some(Arc::new(Answer)));
        let output = tool
            .run(call(), context(CancellationToken::new()))
            .await
            .unwrap();
        let record = output.details.unwrap();
        assert_eq!(record["questions"], arguments()["questions"]);
        assert_eq!(record["response"], output.structured_content.unwrap());
        assert_eq!(record["response"]["answers"]["text"]["answers"], json!([]));
        let cancel = CancellationToken::new();
        cancel.cancel();
        let output = tool.run(call(), context(cancel)).await.unwrap();
        assert!(output.is_error);
        let details = output.details.unwrap();
        assert_eq!(details["status"], "cancelled");
        assert_eq!(details["questions"], arguments()["questions"]);
        assert_eq!(
            details["message"],
            "request_user_input was cancelled before receiving a response."
        );
    }
}
