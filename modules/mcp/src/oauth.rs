//! Profile-local OAuth authorization code flow for Streamable HTTP servers.
use std::fs::{self, File, TryLockError};
#[cfg(unix)]
use std::io::Read;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tiechui_plugin_api::{
    CancellationToken, NetworkConfig, ToolError,
    serde_json::{self, Value, json},
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

fn error(message: impl ToString) -> ToolError {
    ToolError(message.to_string())
}
fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn random() -> Result<String, ToolError> {
    let mut bytes = [0; 32];
    #[cfg(unix)]
    File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    #[cfg(windows)]
    {
        use windows_sys::Win32::Security::Cryptography::{
            BCRYPT_USE_SYSTEM_PREFERRED_RNG, BCryptGenRandom,
        };
        // SAFETY: the buffer is writable and the system RNG needs no algorithm handle.
        let status = unsafe {
            BCryptGenRandom(
                std::ptr::null_mut(),
                bytes.as_mut_ptr(),
                bytes.len() as u32,
                BCRYPT_USE_SYSTEM_PREFERRED_RNG,
            )
        };
        if status < 0 {
            return Err(error(format!("System RNG failed: {status:#x}")));
        }
    }
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn oauth_random_is_a_fresh_256_bit_url_safe_value() -> Result<(), ToolError> {
        let first = random()?;
        assert_ne!(first, random()?);
        assert_eq!(URL_SAFE_NO_PAD.decode(&first).map_err(error)?.len(), 32);
        assert!(
            first
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b))
        );
        Ok(())
    }
}

fn web_url(value: &str) -> Result<Url, ToolError> {
    let url = Url::parse(value).map_err(|_| error("Invalid OAuth URL"))?;
    if (url.scheme() != "https"
        && !(url.scheme() == "http"
            && matches!(url.host_str(), Some("127.0.0.1" | "localhost" | "[::1]"))))
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(error(
            "OAuth requires HTTPS (HTTP is allowed on loopback only)",
        ));
    }
    Ok(url)
}

fn client(network: &NetworkConfig) -> Result<Client, ToolError> {
    network
        .apply(
            Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(30)),
        )
        .map_err(error)?
        .build()
        .map_err(|_| error("Cannot create OAuth HTTP client"))
}

async fn json_response(request: reqwest::RequestBuilder) -> Result<Value, ToolError> {
    let mut response = request
        .send()
        .await
        .map_err(|_| error("OAuth HTTP request failed"))?;
    if !response.status().is_success() {
        return Err(error(format!("OAuth HTTP status {}", response.status())));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| error("OAuth response failed"))?
    {
        if bytes.len() + chunk.len() > 1024 * 1024 {
            return Err(error("OAuth response exceeds size limit"));
        }
        bytes.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&bytes).map_err(|_| error("Invalid OAuth JSON response"))
}

#[derive(Clone, Deserialize, Serialize)]
struct Metadata {
    issuer: String,
    authorization_endpoint: String,
    token_endpoint: String,
    registration_endpoint: Option<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
    #[serde(default)]
    authorization_response_iss_parameter_supported: bool,
}

fn well_known(base: &Url, kind: &str) -> Url {
    let mut url = base.clone();
    url.set_path(&format!(
        "/.well-known/{kind}{}",
        base.path().trim_end_matches('/')
    ));
    url.set_query(None);
    url
}

async fn discover(http: &Client, resource: &str) -> Result<Metadata, ToolError> {
    let base = web_url(resource)?;
    let mut resource_metadata = None;
    // Prefer the server's challenge, then the standard protected-resource location.
    let response = http
        .get(base.clone())
        .send()
        .await
        .map_err(|_| error("OAuth discovery failed"))?;
    if let Some(challenge) = response
        .headers()
        .get("www-authenticate")
        .and_then(|h| h.to_str().ok())
    {
        for part in challenge.split(',') {
            let part = part.trim().strip_prefix("Bearer ").unwrap_or(part.trim());
            if let Some(value) = part.strip_prefix("resource_metadata=") {
                resource_metadata = Some(web_url(value.trim_matches('"'))?);
            }
        }
    }
    let resource_metadata =
        resource_metadata.unwrap_or_else(|| well_known(&base, "oauth-protected-resource"));
    let resource_doc = json_response(http.get(resource_metadata)).await.ok();
    let issuer = if let Some(doc) = resource_doc {
        if doc["resource"]
            .as_str()
            .is_some_and(|value| value != resource)
        {
            return Err(error("OAuth protected resource does not match server URL"));
        }
        let issuer = doc["authorization_servers"]
            .as_array()
            .and_then(|a| a.first())
            .and_then(Value::as_str)
            .ok_or_else(|| error("OAuth resource has no authorization server"))?;
        web_url(issuer)?
    } else {
        let mut issuer = base.clone();
        issuer.set_path("");
        issuer.set_query(None);
        issuer
    };
    let metadata_url = well_known(&issuer, "oauth-authorization-server");
    let doc = match json_response(http.get(metadata_url)).await {
        Ok(doc) => doc,
        Err(_) => {
            let mut oidc = issuer.clone();
            oidc.set_path(&format!(
                "{}/.well-known/openid-configuration",
                issuer.path().trim_end_matches('/')
            ));
            json_response(http.get(oidc)).await?
        }
    };
    let metadata: Metadata =
        serde_json::from_value(doc).map_err(|_| error("Invalid OAuth authorization metadata"))?;
    if web_url(&metadata.issuer)? != issuer {
        return Err(error("OAuth discovery issuer mismatch"));
    }
    let auth = web_url(&metadata.authorization_endpoint)?;
    let token = web_url(&metadata.token_endpoint)?;
    if !metadata.authorization_response_iss_parameter_supported
        && auth.origin() != issuer.origin()
        && auth.origin() != token.origin()
    {
        return Err(error(
            "OAuth authorization endpoint is not bound to its issuer",
        ));
    }
    if let Some(endpoint) = &metadata.registration_endpoint {
        web_url(endpoint)?;
    }
    if !metadata
        .code_challenge_methods_supported
        .iter()
        .any(|s| s == "S256")
    {
        return Err(error("OAuth server must support PKCE S256"));
    }
    Ok(metadata)
}

#[derive(Deserialize, Serialize)]
struct Tokens {
    access_token: String,
    token_type: String,
    refresh_token: Option<String>,
    expires_in: Option<u64>,
}

#[derive(Deserialize, Serialize)]
struct Credentials {
    server_url: String,
    issuer: String,
    client_id: String,
    tokens: Tokens,
    received_at: u64,
}

/// The key binds tokens to both a configured name and its resource URL.
#[derive(Clone)]
pub struct OAuthStore {
    path: PathBuf,
    url: String,
    network: NetworkConfig,
}

impl OAuthStore {
    pub fn new(home: &Path, name: &str, url: &str, network: NetworkConfig) -> Self {
        let hash = Sha256::digest(format!("{name}\0{url}").as_bytes());
        Self {
            path: home.join("mcp-oauth").join(format!("{hash:x}.json")),
            url: url.into(),
            network,
        }
    }

    fn load(&self) -> Result<Option<Credentials>, ToolError> {
        match fs::read(&self.path) {
            Ok(bytes) => {
                let stored: Credentials = serde_json::from_slice(&bytes)
                    .map_err(|_| error("Invalid MCP OAuth token file"))?;
                if stored.server_url != self.url {
                    return Err(error("OAuth token resource mismatch"));
                }
                Ok(Some(stored))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, credentials: &Credentials) -> Result<(), ToolError> {
        let directory = self
            .path
            .parent()
            .ok_or_else(|| error("Missing OAuth directory"))?;
        tiechui_plugin_api::private::create_dir_all(directory)?;
        let mut file = tempfile::NamedTempFile::new_in(directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        file.write_all(
            &serde_json::to_vec(credentials).map_err(|_| error("Cannot serialize OAuth tokens"))?,
        )?;
        file.as_file().sync_all()?;
        file.persist(&self.path)
            .map_err(|_| error("Cannot persist OAuth tokens"))?;
        Ok(())
    }

    async fn lock(&self, cancel: &CancellationToken) -> Result<File, ToolError> {
        tiechui_plugin_api::private::create_dir_all(
            self.path
                .parent()
                .ok_or_else(|| error("Missing OAuth directory"))?,
        )?;
        let file = tiechui_plugin_api::private::open_lock(&self.path.with_extension("lock"))?;
        loop {
            if cancel.is_cancelled() {
                return Err(error("Operation aborted"));
            }
            match file.try_lock() {
                Ok(()) => return Ok(file),
                Err(TryLockError::WouldBlock) => tokio::select! {
                    _ = cancel.cancelled() => return Err(error("Operation aborted")),
                    _ = tokio::time::sleep(Duration::from_millis(20)) => {},
                },
                Err(e) => return Err(error(e)),
            }
        }
    }

    pub fn status(&self) -> &'static str {
        match self.load() {
            Ok(Some(stored))
                if stored
                    .tokens
                    .expires_in
                    .is_some_and(|ttl| now() >= stored.received_at.saturating_add(ttl)) =>
            {
                "expired"
            }
            Ok(Some(_)) => "authenticated",
            Ok(None) => "logged_out",
            Err(_) => "invalid_credentials",
        }
    }

    pub async fn logout(&self, cancel: &CancellationToken) -> Result<(), ToolError> {
        let _lock = self.lock(cancel).await?;
        match fs::remove_file(&self.path) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e.into()),
            _ => Ok(()),
        }
    }

    pub async fn access_token(
        &self,
        cancel: &CancellationToken,
    ) -> Result<Option<String>, ToolError> {
        let operation = async {
            let _lock = self.lock(cancel).await?;
            let Some(mut stored) = self.load()? else {
                return Ok(None);
            };
            if stored.tokens.expires_in.is_some_and(|ttl| {
                now().saturating_add(30) >= stored.received_at.saturating_add(ttl)
            }) {
                let refresh = stored
                    .tokens
                    .refresh_token
                    .as_deref()
                    .ok_or_else(|| error("MCP OAuth token expired; run chui mcp login"))?;
                let http = client(&self.network)?;
                let metadata = discover(&http, &self.url).await?;
                if metadata.issuer != stored.issuer {
                    return Err(error("OAuth issuer changed; run chui mcp login"));
                }
                let refresh = refresh.to_owned();
                let store = self.clone();
                // Once a rotating token may be consumed, finish its bounded save even if
                // the caller cancels. Logout waits on the same lock and cannot resurrect it.
                return tokio::spawn(async move {
                    let _lock = _lock;
                    let mut tokens = exchange(
                        &http,
                        &metadata.token_endpoint,
                        &[
                            ("grant_type", "refresh_token"),
                            ("refresh_token", &refresh),
                            ("client_id", &stored.client_id),
                            ("resource", &store.url),
                        ],
                    )
                    .await?;
                    if tokens.refresh_token.is_none() {
                        tokens.refresh_token = stored.tokens.refresh_token;
                    }
                    stored.tokens = tokens;
                    stored.received_at = now();
                    store.save(&stored)?;
                    Ok(Some(stored.tokens.access_token))
                })
                .await
                .map_err(|_| error("OAuth refresh task failed"))?;
            }
            Ok(Some(stored.tokens.access_token))
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(error("Operation aborted")),
            result = operation => result,
        }
    }
}

async fn exchange(
    http: &Client,
    endpoint: &str,
    fields: &[(&str, &str)],
) -> Result<Tokens, ToolError> {
    let body: String = url_form(fields);
    let value = json_response(
        http.post(endpoint)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(body),
    )
    .await?;
    let tokens: Tokens =
        serde_json::from_value(value).map_err(|_| error("Invalid OAuth token response"))?;
    if !tokens.token_type.eq_ignore_ascii_case("bearer") || tokens.access_token.is_empty() {
        return Err(error("OAuth requires a nonempty bearer token"));
    }
    Ok(tokens)
}

fn url_form(fields: &[(&str, &str)]) -> String {
    let mut url = Url::parse("http://localhost/").unwrap_or_else(|_| unreachable!());
    url.query_pairs_mut().extend_pairs(fields.iter().copied());
    url.query().unwrap_or_default().to_owned()
}

pub struct Login {
    pub authorization_url: String,
    listener: TcpListener,
    callback_path: String,
    redirect_uri: String,
    state: String,
    verifier: String,
    client_id: String,
    metadata: Metadata,
    store: OAuthStore,
    http: Client,
}

impl Login {
    pub async fn begin(
        store: OAuthStore,
        scopes: &[String],
        cancel: &CancellationToken,
    ) -> Result<Self, ToolError> {
        let operation = async {
            let http = client(&store.network)?;
            let metadata = discover(&http, &store.url).await?;
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let callback_path = format!("/callback/{}", random()?);
            let redirect_uri = format!(
                "http://127.0.0.1:{}{callback_path}",
                listener.local_addr()?.port()
            );
            let endpoint = metadata.registration_endpoint.as_deref().ok_or_else(|| {
                error("OAuth server does not support dynamic client registration")
            })?;
            let registration = json_response(http.post(endpoint).json(&json!({
                "client_name":"Tiechui", "redirect_uris":[redirect_uri], "token_endpoint_auth_method":"none",
                "grant_types":["authorization_code", "refresh_token"], "response_types":["code"], "application_type":"native"
            }))).await?;
            let client_id = registration["client_id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .ok_or_else(|| error("OAuth registration returned no client ID"))?
                .to_owned();
            if registration["token_endpoint_auth_method"]
                .as_str()
                .is_some_and(|method| method != "none")
            {
                return Err(error("OAuth server must register a public native client"));
            }
            let verifier = random()?;
            let state = random()?;
            let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
            let mut authorization_url = web_url(&metadata.authorization_endpoint)?;
            let scopes = if scopes.is_empty() {
                metadata.scopes_supported.join(" ")
            } else {
                scopes.join(" ")
            };
            authorization_url.query_pairs_mut().extend_pairs([
                ("response_type", "code"),
                ("client_id", &client_id),
                ("redirect_uri", &redirect_uri),
                ("state", &state),
                ("code_challenge", &challenge),
                ("code_challenge_method", "S256"),
                ("resource", &store.url),
                ("scope", &scopes),
            ]);
            Ok(Self {
                authorization_url: authorization_url.into(),
                listener,
                callback_path,
                redirect_uri,
                state,
                verifier,
                client_id,
                metadata,
                store,
                http,
            })
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(error("Operation aborted")),
            result = operation => result,
        }
    }

    pub async fn finish(self, cancel: &CancellationToken) -> Result<(), ToolError> {
        let operation = async {
            loop {
                let (mut socket, _) = self.listener.accept().await?;
                let mut request = Vec::new();
                let read = async {
                    while request.len() <= 8192 && !request.ends_with(b"\r\n\r\n") {
                        request.push(socket.read_u8().await?);
                    }
                    Ok::<_, std::io::Error>(())
                };
                if !matches!(
                    tokio::time::timeout(Duration::from_secs(2), read).await,
                    Ok(Ok(()))
                ) {
                    continue;
                }
                let request = String::from_utf8_lossy(&request);
                let target = request
                    .lines()
                    .next()
                    .and_then(|line| line.strip_prefix("GET "))
                    .and_then(|line| line.split(' ').next());
                let callback =
                    target.and_then(|target| Url::parse(&format!("http://127.0.0.1{target}")).ok());
                let pairs = callback
                    .as_ref()
                    .map(|url| url.query_pairs().into_owned().collect::<Vec<_>>())
                    .unwrap_or_default();
                let get = |key: &str| {
                    pairs
                        .iter()
                        .find(|(k, _)| k == key)
                        .map(|(_, v)| v.as_str())
                };
                let duplicate = pairs
                    .iter()
                    .enumerate()
                    .any(|(i, (key, _))| pairs[..i].iter().any(|(k, _)| k == key));
                let valid = request.len() <= 8192
                    && !duplicate
                    && callback
                        .as_ref()
                        .is_some_and(|url| url.path() == self.callback_path)
                    && get("state") == Some(self.state.as_str());
                if !valid {
                    socket.write_all(b"HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await?;
                    continue;
                }
                if get("error").is_some() {
                    return Err(error("OAuth authorization was denied"));
                }
                if get("iss").is_some_and(|issuer| issuer != self.metadata.issuer)
                    || (self.metadata.authorization_response_iss_parameter_supported
                        && get("iss").is_none())
                {
                    return Err(error("OAuth callback issuer mismatch"));
                }
                let code = get("code")
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| error("OAuth callback has no code"))?;
                let _lock = self.store.lock(cancel).await?;
                let tokens = exchange(
                    &self.http,
                    &self.metadata.token_endpoint,
                    &[
                        ("grant_type", "authorization_code"),
                        ("code", code),
                        ("redirect_uri", &self.redirect_uri),
                        ("client_id", &self.client_id),
                        ("code_verifier", &self.verifier),
                        ("resource", &self.store.url),
                    ],
                )
                .await?;
                self.store.save(&Credentials {
                    server_url: self.store.url.clone(),
                    issuer: self.metadata.issuer.clone(),
                    client_id: self.client_id.clone(),
                    tokens,
                    received_at: now(),
                })?;
                socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 30\r\nConnection: close\r\n\r\nLogged in. Close this window.\r\n").await?;
                return Ok(());
            }
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(error("Operation aborted")),
            result = tokio::time::timeout(Duration::from_secs(300), operation) => result.map_err(|_| error("OAuth login timed out"))?,
        }
    }
}
