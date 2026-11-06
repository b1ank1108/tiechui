//! Adapt the workspace HTTP client to rmcp without a second TLS stack.

use std::collections::HashMap;
use std::sync::Arc;

use futures::StreamExt;
use reqwest::header::{HeaderName, HeaderValue};
use reqwest::{RequestBuilder, Response, StatusCode};
use rmcp::model::ClientJsonRpcMessage;
use rmcp::transport::common::client_side_sse::BoxedSseResponse;
use rmcp::transport::streamable_http_client::{
    AuthRequiredError, StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
};

type Error = StreamableHttpError<reqwest::Error>;
const DEFAULT_EVENT_LIMIT: usize = 16 * 1024 * 1024;

#[derive(Clone)]
pub(super) struct HttpClient(
    pub reqwest::Client,
    pub Option<crate::oauth::OAuthStore>,
    pub tiechui_plugin_api::CancellationToken,
);

impl HttpClient {
    async fn auth(&self, configured: Option<String>) -> Result<Option<String>, Error> {
        match &self.1 {
            Some(store) if configured.is_none() => store
                .access_token(&self.2)
                .await
                .map_err(|e| Error::Io(std::io::Error::other(e))),
            _ => Ok(configured),
        }
    }
}

fn headers(
    mut request: RequestBuilder,
    session: Option<Arc<str>>,
    auth: Option<String>,
    custom: HashMap<HeaderName, HeaderValue>,
) -> RequestBuilder {
    for (name, value) in custom {
        request = request.header(name, value);
    }
    if let Some(session) = session {
        request = request.header("mcp-session-id", session.as_ref());
    }
    if let Some(token) = auth {
        request = request.bearer_auth(token);
    }
    request
}

async fn send(request: RequestBuilder, session: bool) -> Result<Response, Error> {
    let response = request.send().await.map_err(Error::Client)?;
    if response.status() == StatusCode::NOT_FOUND && session {
        return Err(Error::SessionExpired);
    }
    if response.status() == StatusCode::UNAUTHORIZED {
        let challenge = response
            .headers()
            .get("www-authenticate")
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
            .to_owned();
        return Err(Error::AuthRequired(AuthRequiredError::new(challenge)));
    }
    Ok(response)
}

fn event_stream(response: Response, limit: usize) -> BoxedSseResponse {
    let bytes = async_stream::try_stream! {
        let mut chunks = response.bytes_stream();
        let (mut event, mut line, mut cr) = (0_usize, 0_usize, false);
        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(std::io::Error::other)?;
            for &byte in &chunk {
                if cr && byte == b'\n' { cr = false; continue; }
                cr = byte == b'\r';
                if matches!(byte, b'\r' | b'\n') {
                    event = if line == 0 { 0 } else { event.saturating_add(line).saturating_add(1) };
                    line = 0;
                } else { line = line.saturating_add(1); }
                if event.saturating_add(line) > limit {
                    Err(std::io::Error::other("MCP SSE event exceeds size limit"))?;
                }
            }
            yield chunk;
        }
    };
    sse_stream::SseStream::from_bytes_stream(bytes.map(|item: Result<_, std::io::Error>| item))
        .boxed()
}

impl StreamableHttpClient for HttpClient {
    type Error = reqwest::Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        custom: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, Error> {
        self.post_message_with_max_sse_event_size(
            uri,
            message,
            session,
            auth,
            custom,
            DEFAULT_EVENT_LIMIT,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session: Option<Arc<str>>,
        auth: Option<String>,
        custom: HashMap<HeaderName, HeaderValue>,
        max: usize,
    ) -> Result<StreamableHttpPostResponse, Error> {
        let expects_reply = matches!(message, ClientJsonRpcMessage::Request(_));
        let auth = self.auth(auth).await?;
        let attached = session.is_some();
        let request = headers(self.0.post(uri.as_ref()), session, auth, custom)
            .header("accept", "application/json, text/event-stream")
            .json(&message);
        let response = send(request, attached)
            .await?
            .error_for_status()
            .map_err(Error::Client)?;
        if matches!(
            response.status(),
            StatusCode::ACCEPTED | StatusCode::NO_CONTENT
        ) || (!expects_reply && response.content_length() == Some(0))
        {
            return Ok(StreamableHttpPostResponse::Accepted);
        }
        let session = response
            .headers()
            .get("mcp-session-id")
            .and_then(|s| s.to_str().ok())
            .map(str::to_owned);
        let mime = response
            .headers()
            .get("content-type")
            .and_then(|s| s.to_str().ok())
            .map(str::to_owned);
        match mime
            .as_deref()
            .and_then(|s| s.split(';').next())
            .map(str::trim)
        {
            Some("text/event-stream") => Ok(StreamableHttpPostResponse::Sse(
                event_stream(response, max),
                session,
            )),
            Some("application/json") => {
                let bytes = response.bytes().await.map_err(Error::Client)?;
                match tiechui_plugin_api::serde_json::from_slice(&bytes) {
                    Ok(message) => Ok(StreamableHttpPostResponse::Json(message, session)),
                    Err(_) if !expects_reply => Ok(StreamableHttpPostResponse::Accepted),
                    Err(error) => Err(Error::Deserialize(error)),
                }
            }
            _ => Err(Error::UnexpectedContentType(mime)),
        }
    }

    async fn get_stream(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        auth: Option<String>,
        custom: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, Error> {
        self.get_stream_with_max_sse_event_size(
            uri,
            session,
            last,
            auth,
            custom,
            DEFAULT_EVENT_LIMIT,
        )
        .await
    }

    async fn get_stream_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        session: Option<Arc<str>>,
        last: Option<String>,
        auth: Option<String>,
        custom: HashMap<HeaderName, HeaderValue>,
        max: usize,
    ) -> Result<BoxedSseResponse, Error> {
        let attached = session.is_some();
        let auth = self.auth(auth).await?;
        let mut request = headers(self.0.get(uri.as_ref()), session, auth, custom)
            .header("accept", "text/event-stream");
        if let Some(last) = last {
            request = request.header("last-event-id", last);
        }
        let response = send(request, attached).await?;
        if response.status() == StatusCode::METHOD_NOT_ALLOWED {
            return Err(Error::ServerDoesNotSupportSse);
        }
        let response = response.error_for_status().map_err(Error::Client)?;
        Ok(event_stream(response, max))
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session: Arc<str>,
        auth: Option<String>,
        custom: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), Error> {
        let auth = self.auth(auth).await?;
        let response = send(
            headers(self.0.delete(uri.as_ref()), Some(session), auth, custom),
            true,
        )
        .await?;
        if response.status() != StatusCode::METHOD_NOT_ALLOWED {
            response.error_for_status().map_err(Error::Client)?;
        }
        Ok(())
    }
}
