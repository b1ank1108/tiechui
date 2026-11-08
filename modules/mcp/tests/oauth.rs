#![allow(clippy::unwrap_used)]
mod support;

use axum::{
    Json, Router,
    extract::State,
    http::{Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use reqwest::Url;
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs,
    path::Path,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
};
use tiechui_module_mcp::{
    DeclineElicitation, McpModule, ServerState,
    oauth::{Login, OAuthStore},
};
use tiechui_plugin_api::{
    CancellationToken, McpOrigin, McpServerConfig, McpServerDefinition, Module, ModuleConfig,
    Registry,
    serde_json::{self, Value, json},
};

#[derive(Clone)]
struct Server {
    base: String,
    redirect: Arc<Mutex<String>>,
    challenge: Arc<Mutex<String>>,
    refreshes: Arc<AtomicUsize>,
    exchanges: Arc<AtomicUsize>,
    wrong_issuer: Arc<AtomicUsize>,
    refresh_delay: Arc<AtomicUsize>,
}

async fn metadata(State(s): State<Server>) -> Json<Value> {
    Json(
        json!({"issuer":if s.wrong_issuer.load(Ordering::SeqCst) == 0 { s.base.clone() } else { format!("{}/changed",s.base) },
        "authorization_endpoint":format!("{}/authorize",s.base),"token_endpoint":format!("{}/token",s.base),
        "registration_endpoint":format!("{}/register",s.base),"code_challenge_methods_supported":["S256"],
        "scopes_supported":["tools"],"authorization_response_iss_parameter_supported":true}),
    )
}
async fn register(State(s): State<Server>, Json(body): Json<Value>) -> Json<Value> {
    assert_eq!(body["token_endpoint_auth_method"], "none");
    assert_eq!(body["application_type"], "native");
    *s.redirect.lock().unwrap() = body["redirect_uris"][0].as_str().unwrap().into();
    Json(json!({"client_id":"fixture-client","token_endpoint_auth_method":"none"}))
}
async fn token(State(s): State<Server>, body: String) -> Json<Value> {
    let fields: BTreeMap<String, String> = Url::parse(&format!("http://localhost/?{body}"))
        .unwrap()
        .query_pairs()
        .into_owned()
        .collect();
    assert_eq!(fields["resource"], format!("{}/mcp", s.base));
    assert_eq!(fields["client_id"], "fixture-client");
    if fields["grant_type"] == "authorization_code" {
        assert_eq!(fields["code"], "fixture-code");
        assert_eq!(fields["redirect_uri"], *s.redirect.lock().unwrap());
        assert_eq!(
            URL_SAFE_NO_PAD.encode(Sha256::digest(fields["code_verifier"].as_bytes())),
            *s.challenge.lock().unwrap()
        );
        s.exchanges.fetch_add(1, Ordering::SeqCst);
        Json(
            json!({"access_token":"initial-token","token_type":"Bearer","refresh_token":"first-refresh","expires_in":3600}),
        )
    } else {
        assert_eq!(fields["grant_type"], "refresh_token");
        assert_eq!(fields["refresh_token"], "first-refresh");
        s.refreshes.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(
            s.refresh_delay.load(Ordering::SeqCst) as u64,
        ))
        .await;
        Json(
            json!({"access_token":"refreshed-token","token_type":"Bearer","refresh_token":"rotated-refresh","expires_in":3600}),
        )
    }
}
async fn authorize_mcp(
    State(s): State<Server>,
    request: Request<axum::body::Body>,
    next: Next,
) -> Response {
    if request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        == Some("Bearer refreshed-token")
    {
        next.run(request).await
    } else {
        (
            StatusCode::UNAUTHORIZED,
            [(
                "www-authenticate",
                format!("Bearer resource_metadata=\"{}/resource\"", s.base),
            )],
        )
            .into_response()
    }
}

struct Fixture {
    temp: tempfile::TempDir,
    server: Server,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.task.abort();
    }
}
impl Fixture {
    async fn new() -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/oauth-tests");
        fs::create_dir_all(&root).unwrap();
        let temp = tempfile::tempdir_in(root).unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let server = Server {
            base: format!("http://{}", listener.local_addr().unwrap()),
            redirect: Arc::default(),
            challenge: Arc::default(),
            refreshes: Arc::default(),
            exchanges: Arc::default(),
            wrong_issuer: Arc::default(),
            refresh_delay: Arc::default(),
        };
        let resource =
            json!({"resource":format!("{}/mcp",server.base),"authorization_servers":[server.base]});
        let mcp = rmcp::transport::streamable_http_server::StreamableHttpService::new(
            || Ok(support::TestServer::default()), Arc::new(rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default()), Default::default());
        let protected =
            Router::new()
                .nest_service("/mcp", mcp)
                .layer(middleware::from_fn_with_state(
                    server.clone(),
                    authorize_mcp,
                ));
        let app = Router::new()
            .merge(protected)
            .route("/resource", get(move || async { Json(resource) }))
            .route("/.well-known/oauth-authorization-server", get(metadata))
            .route("/register", post(register))
            .route("/token", post(token))
            .with_state(server.clone());
        let task = tokio::spawn(async {
            axum::serve(listener, app).await.unwrap();
        });
        Self { temp, server, task }
    }
    fn url(&self) -> String {
        format!("{}/mcp", self.server.base)
    }
    fn store(&self) -> OAuthStore {
        OAuthStore::new(self.temp.path(), "fixture", &self.url(), Default::default())
    }
    async fn begin(&self) -> (Login, Url) {
        let login = Login::begin(self.store(), &[], &CancellationToken::new())
            .await
            .unwrap();
        let authorization = Url::parse(&login.authorization_url).unwrap();
        let fields: BTreeMap<_, _> = authorization.query_pairs().into_owned().collect();
        assert_eq!(fields["code_challenge_method"], "S256");
        assert_eq!(fields["resource"], self.url());
        assert_eq!(fields["scope"], "tools");
        *self.server.challenge.lock().unwrap() = fields["code_challenge"].clone();
        let mut callback = Url::parse(&fields["redirect_uri"]).unwrap();
        callback.query_pairs_mut().extend_pairs([
            ("code", "fixture-code"),
            ("state", fields["state"].as_str()),
            ("iss", self.server.base.as_str()),
        ]);
        (login, callback)
    }
    async fn login(&self) {
        let (login, callback) = self.begin().await;
        let cancel = CancellationToken::new();
        let (result, response) = tokio::join!(login.finish(&cancel), reqwest::get(callback));
        result.unwrap();
        assert!(response.unwrap().status().is_success());
    }
    fn token_path(&self) -> std::path::PathBuf {
        fs::read_dir(self.temp.path().join("mcp-oauth"))
            .unwrap()
            .map(|e| e.unwrap().path())
            .find(|p| p.extension().is_some_and(|s| s == "json"))
            .unwrap()
    }
    fn expire(&self, refresh: bool) {
        let path = self.token_path();
        let mut value: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["received_at"] = json!(0);
        if !refresh {
            value["tokens"]["refresh_token"] = Value::Null;
        }
        fs::write(path, value.to_string()).unwrap();
    }
}

#[tokio::test]
async fn login_refresh_rotation_permissions_status_transport_and_logout() {
    let f = Fixture::new().await;
    let cancel = CancellationToken::new();
    assert_eq!(f.store().status(), "logged_out");
    f.login().await;
    assert_eq!(f.store().status(), "authenticated");
    assert_eq!(
        f.store().access_token(&cancel).await.unwrap().as_deref(),
        Some("initial-token")
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            fs::metadata(f.token_path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    f.expire(true);
    assert_eq!(f.store().status(), "expired");
    let store = f.store();
    let (one, two) = tokio::join!(store.access_token(&cancel), store.access_token(&cancel));
    assert_eq!(one.unwrap(), two.unwrap());
    assert_eq!(f.server.refreshes.load(Ordering::SeqCst), 1);
    let tokens: Value = serde_json::from_slice(&fs::read(f.token_path()).unwrap()).unwrap();
    assert_eq!(tokens["tokens"]["refresh_token"], "rotated-refresh");
    let module = McpModule::start_with_profile(
        vec![McpServerDefinition {
            name: "fixture".into(),
            origin: McpOrigin::Extension("fixture".into()),
            config: McpServerConfig {
                url: Some(f.url()),
                ..Default::default()
            },
        }],
        f.temp.path(),
        true,
        cancel.clone(),
        Arc::new(DeclineElicitation),
        Some(f.temp.path()),
        Default::default(),
    );
    module.wait_for_startup(cancel.clone()).await;
    assert_eq!(
        module.statuses()[0].state,
        ServerState::Ready,
        "{:?}",
        module.statuses()
    );
    assert_eq!(module.statuses()[0].oauth.as_deref(), Some("authenticated"));
    let mut registry = Registry::default();
    module
        .register(&mut registry, &ModuleConfig::default())
        .unwrap();
    let status = registry.commands()[0]
        .run("", cancel.clone())
        .await
        .unwrap();
    assert!(status.contains("OAuth: authenticated"));
    assert!(!status.contains("refreshed-token"));
    store.logout(&cancel).await.unwrap();
    assert_eq!(module.statuses()[0].oauth.as_deref(), Some("logged_out"));
    assert!(store.access_token(&cancel).await.unwrap().is_none());
    store.logout(&cancel).await.unwrap();
    cancel.cancel();
}

#[tokio::test]
async fn expiry_without_refresh_and_changed_issuer_require_login() {
    let f = Fixture::new().await;
    let cancel = CancellationToken::new();
    f.login().await;
    f.expire(false);
    assert!(
        f.store()
            .access_token(&cancel)
            .await
            .unwrap_err()
            .0
            .contains("expired")
    );
    f.login().await;
    f.expire(true);
    f.server.wrong_issuer.store(1, Ordering::SeqCst);
    assert!(f.store().access_token(&cancel).await.is_err());
    assert_eq!(f.server.refreshes.load(Ordering::SeqCst), 0);
    assert_eq!(
        OAuthStore::new(
            f.temp.path(),
            "fixture",
            "http://127.0.0.1:1/mcp",
            Default::default()
        )
        .status(),
        "logged_out"
    );
}

#[tokio::test]
async fn cancelled_refresh_persists_rotation_before_logout() {
    let f = Fixture::new().await;
    f.login().await;
    f.expire(true);
    f.server.refresh_delay.store(200, Ordering::SeqCst);
    let cancel = CancellationToken::new();
    let worker_cancel = cancel.clone();
    let store = f.store();
    let worker = tokio::spawn(async move { store.access_token(&worker_cancel).await });
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while f.server.refreshes.load(Ordering::SeqCst) == 0 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    assert!(
        tokio::time::timeout(std::time::Duration::from_millis(100), worker)
            .await
            .unwrap()
            .unwrap()
            .is_err()
    );
    f.store().logout(&CancellationToken::new()).await.unwrap();
    assert_eq!(f.store().status(), "logged_out");
    assert_eq!(f.server.refreshes.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn invalid_state_and_path_do_not_exchange_code_and_cancellation_closes_listener() {
    let f = Fixture::new().await;
    let (login, callback) = f.begin().await;
    let cancel = CancellationToken::new();
    let waiter_cancel = cancel.clone();
    let waiter = tokio::spawn(async move { login.finish(&waiter_cancel).await });
    let mut wrong = callback.clone();
    wrong.set_query(Some("code=fixture-code&state=wrong"));
    assert_eq!(
        reqwest::get(wrong).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    let mut wrong = callback.clone();
    wrong.set_path("/callback/wrong");
    assert_eq!(
        reqwest::get(wrong).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(f.server.exchanges.load(Ordering::SeqCst), 0);
    cancel.cancel();
    assert!(waiter.await.unwrap().is_err());
    assert!(reqwest::get(callback).await.is_err());
    assert_eq!(f.store().status(), "logged_out");
}

#[tokio::test]
async fn issuer_bound_callback_rejects_missing_or_wrong_issuer() {
    for issuer in [None, Some("http://127.0.0.1:1")] {
        let f = Fixture::new().await;
        let (login, mut callback) = f.begin().await;
        let fields: Vec<_> = callback
            .query_pairs()
            .filter(|(key, _)| key != "iss")
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        callback.set_query(None);
        callback.query_pairs_mut().extend_pairs(fields);
        if let Some(issuer) = issuer {
            callback.query_pairs_mut().append_pair("iss", issuer);
        }
        let cancel = CancellationToken::new();
        let (result, _) = tokio::join!(login.finish(&cancel), reqwest::get(callback));
        assert!(result.unwrap_err().0.contains("issuer mismatch"));
        assert_eq!(f.server.exchanges.load(Ordering::SeqCst), 0);
    }
}
