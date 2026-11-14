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
        let (provider, id) = model
            .split_once('/')
            .ok_or_else(|| ToolError(format!("Expected <provider>/<id>, got {model}")))?;
        let provider = self
            .providers
            .get(provider)
            .ok_or_else(|| ToolError(format!("Provider {provider} is not loaded")))?;
        request.model = ModelId(id.into());
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
