use futures::FutureExt;
use std::collections::BTreeMap;
use std::future::Future;
use std::sync::Arc;

use futures::StreamExt;
use tiechui_model::{ModelId, ModelRequest, ToolCall};
use tiechui_plugin_api::{
    BeforeToolCall, CancellationToken, InterceptorStage, RegisteredInterceptor, Registry,
    async_trait,
};
use tiechui_providers::ModelProvider;
use tiechui_tools::{
    Concurrency, ModelStream, Tool, ToolContext, ToolError, ToolInvoker, ToolOutput, ToolScheduler,
};

use crate::{error_output, validation};

pub(crate) struct Invoker {
    pub registry: Arc<Registry>,
    pub scheduler: Arc<ToolScheduler>,
    pub model: Option<(ModelId, Arc<dyn ModelProvider>)>,
    pub providers: BTreeMap<String, Arc<dyn ModelProvider>>,
}

async fn stream_on(
    provider: &Arc<dyn ModelProvider>,
    request: ModelRequest,
    cancel: CancellationToken,
) -> Result<ModelStream, ToolError> {
    let stream = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(ToolError("Operation aborted".into())),
        result = provider.stream(request, cancel.clone()) => result.map_err(|error| ToolError(error.to_string()))?,
    };
    Ok(stream
        .map(|event| event.map_err(|error| ToolError(error.to_string())))
        .boxed())
}

#[async_trait]
impl ToolInvoker for Invoker {
    fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.registry.tools().get(name)
    }
    async fn invoke(&self, call: ToolCall, ctx: ToolContext) -> ToolOutput {
        execute_call(&self.registry, &self.scheduler, call, ctx).await
    }
    fn model(&self) -> Option<ModelId> {
        self.model.as_ref().map(|(model, _)| model.clone())
    }
    async fn model_stream(
        &self,
        request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ToolError> {
        let (_, provider) = self
            .model
            .as_ref()
            .ok_or_else(|| ToolError("Model access is unavailable".into()))?;
        stream_on(provider, request, cancel).await
    }
    async fn model_stream_on(
        &self,
        model: &str,
        mut request: ModelRequest,
        cancel: CancellationToken,
    ) -> Result<ModelStream, ToolError> {
        let (provider, _) = model
            .split_once('/')
            .ok_or_else(|| ToolError(format!("Expected <provider>/<id>, got {model}")))?;
        let provider = self
            .providers
            .get(provider)
            .ok_or_else(|| ToolError(format!("Provider {provider} is not loaded")))?;
        // Providers key their models by the full `<provider>/<id>`.
        request.model = ModelId(model.into());
        stream_on(provider, request, cancel).await
    }
}

async fn bounded<T>(
    registration: &RegisteredInterceptor,
    ctx: &ToolContext,
    callback: impl FnOnce(ToolContext) -> T,
) -> Result<<T as Future>::Output, ToolError>
where
    T: Future,
{
    let mut callback_ctx = ctx.clone();
    callback_ctx.cancel = ctx.cancel.child_token();
    let cancel = callback_ctx.cancel.clone();
    let result = tokio::select! {
        biased;
        _ = ctx.cancel.cancelled() => Err(ToolError("Operation aborted".into())),
        result = tokio::time::timeout(registration.timeout, callback(callback_ctx)) => {
            result.map_err(|_| ToolError("Tool interceptor timed out".into()))
        }
    };
    cancel.cancel();
    result
}

/// The only dispatch path: local and future remote tools use the same registry.
pub(crate) async fn execute_call(
    registry: &Registry,
    scheduler: &ToolScheduler,
    call: ToolCall,
    ctx: ToolContext,
) -> ToolOutput {
    // Settlement still runs after a tool panic, so leases cannot outlive the call.
    let mut output = match std::panic::AssertUnwindSafe(execute_inner(
        registry,
        scheduler,
        call.clone(),
        ctx.clone(),
    ))
    .catch_unwind()
    .await
    {
        Ok(output) => output,
        Err(_) => {
            ctx.cancel.cancel();
            ToolOutput {
                details: Some(serde_json::json!({"panic":true})),
                ..error_output("Tool panicked; execution isolated.")
            }
        }
    };
    for registration in registry.interceptors() {
        let mut settlement = ctx.clone();
        settlement.cancel = tiechui_plugin_api::CancellationToken::new();
        match bounded(registration, &settlement, |ctx| {
            registration.interceptor.settled(&call, ctx)
        })
        .await
        {
            Ok(Ok(())) => {}
            Ok(Err(error)) | Err(error) => {
                output = error_output(format!("Tool settlement failed: {error}"))
            }
        }
    }
    output
}

async fn execute_inner(
    registry: &Registry,
    scheduler: &ToolScheduler,
    mut call: ToolCall,
    ctx: ToolContext,
) -> ToolOutput {
    // Policies inspect coerced arguments whenever the original call is valid.
    if let Ok(arguments) = validation::validate(registry.tools(), &call) {
        call.arguments = arguments;
    }
    let delegated = registry
        .tools()
        .get(&call.name)
        .is_some_and(|tool| tool.concurrency(&call.arguments) == Concurrency::Delegated);
    for registration in registry.interceptors() {
        if delegated && registration.interceptor.stage() == InterceptorStage::Policy {
            continue;
        }
        match bounded(registration, &ctx, |ctx| {
            registration.interceptor.before(&call, ctx)
        })
        .await
        {
            Ok(Ok(BeforeToolCall::Allow)) => {}
            Ok(Ok(BeforeToolCall::Deny(message))) => return error_output(message),
            Ok(Ok(BeforeToolCall::Rewrite(arguments))) => {
                call.arguments = arguments;
                match validation::validate(registry.tools(), &call) {
                    Ok(arguments) => call.arguments = arguments,
                    Err(error) => return error_output(error),
                }
            }
            Ok(Err(error)) | Err(error) => {
                return error_output(format!("Tool interceptor failed: {error}"));
            }
        }
    }
    let mut output = match validation::validate(registry.tools(), &call) {
        Err(error) => error_output(error),
        Ok(arguments) => {
            call.arguments = arguments;
            match scheduler
                .run_batch(registry.tools(), vec![call.clone()], ctx.clone())
                .await
                .pop()
            {
                Some(Ok(output)) => output,
                Some(Err(error)) => error_output(error.to_string()),
                None => error_output("missing tool result"),
            }
        }
    };
    // Preserve the tool's settled cancellation result; callbacks cannot run
    // with a cancelled context and would replace it with a generic error.
    if ctx.cancel.is_cancelled() {
        return output;
    }
    for registration in registry.interceptors() {
        if delegated && registration.interceptor.stage() == InterceptorStage::Policy {
            continue;
        }
        match bounded(registration, &ctx, |ctx| {
            registration.interceptor.after(&call, &output, ctx)
        })
        .await
        {
            Ok(Ok(Some(replacement))) => output = replacement,
            Ok(Ok(None)) => {}
            Ok(Err(error)) | Err(error) => {
                return error_output(format!("Tool interceptor failed: {error}"));
            }
        }
    }
    output
}

#[cfg(test)]
mod tests {
    use super::*;
    use tiechui_config::{ModelConfig, ProviderConfig, Wire};
    use wiremock::{Mock, MockServer, ResponseTemplate, matchers::method};

    #[tokio::test]
    async fn model_stream_on_reaches_another_loaded_provider() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                include_str!(
                    "../../tiechui-providers/tests/fixtures/openai-completions/plain_text.sse"
                ),
                "text/event-stream",
            ))
            .expect(1)
            .mount(&server)
            .await;
        let provider = tiechui_providers::testing::provider(
            "beta",
            &ProviderConfig {
                wire: Wire::OpenaiCompletions,
                base_url: Some(server.uri()),
                api_key: None,
                api_key_env: None,
                compat: Default::default(),
                models: vec![
                    serde_json::from_value::<ModelConfig>(
                        serde_json::json!({"id":"fast","context_window":200_000,"max_output":4096}),
                    )
                    .unwrap(),
                ],
            },
        )
        .unwrap();
        let invoker = Invoker {
            registry: Arc::new(Registry::default()),
            scheduler: Arc::new(ToolScheduler::default()),
            model: None,
            providers: BTreeMap::from([("beta".into(), Arc::from(provider))]),
        };
        let request = || ModelRequest {
            disable_cache: true,
            short_cache: false,
            cache_key: None,
            model: ModelId("alpha/heavy".into()),
            messages: vec![tiechui_model::Message {
                role: tiechui_model::Role::User,
                content: vec![tiechui_model::Content::Text {
                    text: "review".into(),
                    opaque: None,
                }],
                opaque: None,
            }],
            input_tokens: None,
            tools: vec![],
            max_output: Some(16),
            reasoning: None,
        };
        let stream = invoker
            .model_stream_on("beta/fast", request(), CancellationToken::new())
            .await
            .unwrap();
        assert!(stream.collect::<Vec<_>>().await.iter().all(Result::is_ok));
        let received = server.received_requests().await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&received[0].body).unwrap();
        assert_eq!(body["model"], "fast");
        assert!(matches!(
            invoker
                .model_stream_on("gamma/fast", request(), CancellationToken::new())
                .await,
            Err(ToolError(message)) if message == "Provider gamma is not loaded"
        ));
    }
}
