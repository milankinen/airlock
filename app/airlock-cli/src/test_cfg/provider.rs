//! A fake OAuth provider behind a network service: the token host and the
//! API host of Anthropic or OpenAI as local TLS upstreams, the service
//! built against them, and the guest's side of the sign-in, the token
//! requests and the API calls through the proxy.

use std::io::Write as _;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use airlock_common::network_capnp::network_proxy;
use axum::Router;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use bytes::Bytes;
use http_body_util::Full;
use hyper::Request;
use serde_json::{Value, json};
use sha2::{Digest as _, Sha256};

use super::context::{StoreHome, test_store};
use super::network::{TestNetworkConfig, run_with_config};
use super::upstream::{FakeUpstream, GuestResponse, Seen, SeenLog, guest_request, post};
use crate::network::interceptor::Interceptor;
use crate::services::auth_codes::{Channel, PendingCodes};
use crate::services::store::{GrantSummary, TokenStore, list_grants, now_ms};
use crate::services::{ServiceId, anthropic, openai};

pub type Proxy = network_proxy::Client;

/// The code the fake provider issues: what the browser brings back.
pub const REAL_CODE: &str = "real-code";
/// The code of OpenAI's device-code sign-in.
pub const REAL_DEVICE_CODE: &str = "real-device-code";
/// The API key Anthropic's `create_api_key` issues.
pub const REAL_API_KEY: &str = "sk-ant-api03-REAL-KEY";
/// Claude Code's OAuth client of the Claude.ai sign-in.
pub const CLAUDE_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Claude Code's OAuth client of the Anthropic Console sign-in.
pub const CONSOLE_CLIENT_ID: &str = "41077d10-94b8-4194-be48-d251e9eb21b4";
/// Codex's OAuth client.
pub const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// The scopes of an Anthropic sign-in.
pub const CLAUDE_SCOPE: &str = "org:create_api_key user:inference user:profile";
/// The `redirect_uri` of Claude's manual sign-in.
pub const CLAUDE_MANUAL: &str = "https://platform.claude.com/oauth/code/callback";
/// The `redirect_uri` of Codex's device-code sign-in.
pub const CODEX_DEVICE: &str = "https://auth.openai.com/deviceauth/callback";
/// The PKCE verifier of every exchange the guest sends.
pub const VERIFIER: &str = "v";

/// The S256 PKCE challenge of `verifier`.
pub fn challenge_of(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// Claude's authorize page with `client_id` and `scope` (URL-encoded),
/// its callback on `port`, and the challenge of `verifier`.
pub fn claude_sign_in_page(client_id: &str, scope: &str, port: u16, verifier: &str) -> url::Url {
    url::Url::parse(&format!(
        "https://platform.claude.com/oauth/authorize?code=true&client_id={client_id}\
         &response_type=code&redirect_uri=http%3A%2F%2Flocalhost%3A{port}%2Fcallback\
         &scope={scope}&code_challenge={}&code_challenge_method=S256&state=s",
        challenge_of(verifier)
    ))
    .unwrap()
}

/// Where the API host of the fake provider is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ApiHost {
    /// On the token host's upstream (Anthropic only).
    TokenHost,
    /// On an upstream of its own.
    Own,
    /// On an upstream of its own that is a WebSocket endpoint
    /// ([`super::upstream::upgrade_echo`]) instead of the provider.
    WebSocket,
}

/// How the fake provider and its hosts behave.
#[derive(Clone)]
pub struct Options {
    /// Where the API host is; OpenAI's is never the token host's.
    pub api: ApiHost,
    /// The lifetime of the access tokens: `expires_in`, and the `exp` of
    /// OpenAI's.
    pub expires_in: i64,
    /// Anthropic's exchange names the account.
    pub account: bool,
    /// Token answers are gzip-compressed.
    pub gzip: bool,
    /// The answer status of a revoke.
    pub revoke_status: u16,
    /// How long a revoke takes.
    pub revoke_delay: Duration,
    /// Fields the code exchange answer carries besides (or instead of)
    /// the usual ones.
    pub exchange_extra: Option<Value>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            api: ApiHost::TokenHost,
            expires_in: 3600,
            account: true,
            gzip: false,
            revoke_status: 200,
            revoke_delay: Duration::ZERO,
            exchange_extra: None,
        }
    }
}

/// The provider's side: the tokens it accepts right now, and every
/// request it got.
pub struct FakeProvider {
    service: ServiceId,
    opts: Options,
    pub seen: SeenLog,
    access: Mutex<String>,
    refresh: Mutex<String>,
    refreshes: AtomicUsize,
    /// The plan in the ID token an OpenAI refresh issues (none: no ID
    /// token).
    pub refreshed_plan: Mutex<Option<String>>,
    /// Fields a refresh answer carries besides the tokens.
    pub refresh_extra: Mutex<Option<Value>>,
    /// Holds the next refresh until released (none: refreshes answer at
    /// once).
    refresh_gate: Mutex<Option<RefreshGate>>,
}

/// A refresh held by the provider: `arrived` fires when it reaches the
/// token endpoint, and it is answered once `release` fires.
#[derive(Clone, Default)]
pub struct RefreshGate {
    pub arrived: Arc<tokio::sync::Notify>,
    pub release: Arc<tokio::sync::Notify>,
}

/// A JWT with `claims` and a signature that marks it as real.
fn real_jwt(claims: &Value, n: usize) -> String {
    format!(
        "{}.{}.REALSIG{n}",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string()),
    )
}

/// The claims of a JWT.
pub fn claims_of(token: &str) -> Value {
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
}

/// The fields of a form body.
pub fn form_fields(body: &str) -> std::collections::BTreeMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

/// A gzip-compressed answer of `content_type`, whatever the request
/// accepts.
fn gzipped(content_type: &'static str, body: &str) -> Response {
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(body.as_bytes()).unwrap();
    (
        [("content-type", content_type), ("content-encoding", "gzip")],
        gz.finish().unwrap(),
    )
        .into_response()
}

impl FakeProvider {
    fn new(service: ServiceId, opts: Options) -> Arc<Self> {
        let fake = Self {
            service,
            opts,
            seen: SeenLog::default(),
            access: Mutex::new(String::new()),
            refresh: Mutex::new(String::new()),
            refreshes: AtomicUsize::new(0),
            refreshed_plan: Mutex::new(None),
            refresh_extra: Mutex::new(None),
            refresh_gate: Mutex::new(None),
        };
        fake.rotate(0);
        Arc::new(fake)
    }

    /// Issue the `n`th access and refresh tokens.
    fn rotate(&self, n: usize) {
        let (access, refresh) = match self.service {
            ServiceId::Anthropic => (
                format!("sk-ant-oat01-REAL-ACCESS-{n}"),
                format!("sk-ant-ort01-REAL-REFRESH-{n}"),
            ),
            ServiceId::Openai => (
                real_jwt(
                    &json!({
                        "exp": now_ms() / 1000 + self.opts.expires_in,
                        "https://api.openai.com/auth": { "chatgpt_plan_type": "plus" },
                    }),
                    n,
                ),
                format!("v1.REAL-RT-{n}-{}", "x_Y-".repeat(15)),
            ),
        };
        *self.access.lock().unwrap() = access;
        *self.refresh.lock().unwrap() = refresh;
    }

    /// The real access token the provider accepts now.
    pub fn access(&self) -> String {
        self.access.lock().unwrap().clone()
    }

    /// The real refresh token the provider accepts now.
    pub fn refresh(&self) -> String {
        self.refresh.lock().unwrap().clone()
    }

    pub fn set_access(&self, token: &str) {
        *self.access.lock().unwrap() = token.into();
    }

    pub fn set_refresh(&self, token: &str) {
        *self.refresh.lock().unwrap() = token.into();
    }

    /// Hold the next refresh until the returned gate is released.
    pub fn hold_next_refresh(&self) -> RefreshGate {
        let gate = RefreshGate::default();
        *self.refresh_gate.lock().unwrap() = Some(gate.clone());
        gate
    }

    /// The successful refreshes so far.
    pub fn refreshes(&self) -> usize {
        self.refreshes.load(Ordering::SeqCst)
    }

    /// OpenAI's ID token with the plan `plan`.
    pub fn id_token(plan: &str) -> String {
        real_jwt(
            &json!({
                "email": "o@example.com",
                "sub": "user-1",
                "exp": 4_102_444_800_i64,
                "https://api.openai.com/auth": {
                    "chatgpt_account_id": "acc-1",
                    "chatgpt_plan_type": plan,
                },
            }),
            0,
        )
    }

    fn app(self: &Arc<Self>) -> Router {
        let fake = self.clone();
        Router::new().fallback(move |req: axum::extract::Request| {
            let fake = fake.clone();
            async move { fake.handle(req).await }
        })
    }

    async fn handle(&self, req: axum::extract::Request) -> Response {
        let seen = self.seen.record(req).await;
        let post = seen.method == "POST";
        let path = seen.path.as_str();
        if post && path == token_path(self.service) {
            let gate = if seen.body.contains("refresh_token") {
                self.refresh_gate.lock().unwrap().take()
            } else {
                None
            };
            if let Some(gate) = gate {
                gate.arrived.notify_one();
                gate.release.notified().await;
            }
            return self.token(&seen);
        }
        if post && path == revoke_path(self.service) {
            tokio::time::sleep(self.opts.revoke_delay).await;
            let status = StatusCode::from_u16(self.opts.revoke_status).unwrap();
            return (status, axum::Json(json!({}))).into_response();
        }
        if post && path == "/api/accounts/deviceauth/token" {
            return axum::Json(json!({
                "authorization_code": REAL_DEVICE_CODE,
                "code_challenge": "c",
                "code_verifier": VERIFIER,
            }))
            .into_response();
        }
        let bearer = format!("Bearer {}", self.access());
        let authorized = seen.header("authorization") == Some(bearer.as_str())
            || seen.header("x-api-key") == Some(REAL_API_KEY);
        if !authorized {
            return (StatusCode::UNAUTHORIZED, "no").into_response();
        }
        match path {
            "/api/oauth/claude_cli/create_api_key" => {
                self.json(json!({ "raw_key": REAL_API_KEY, "name": "claude-code" }))
            }
            p if p.ends_with("/gzip/json") => gzipped("application/json", "{}"),
            p if p.ends_with("/gzip/sse") => gzipped("text/event-stream", "data: {}\n\n"),
            p if p.ends_with("/leak/access") => {
                axum::Json(json!({ "t": [self.access()] })).into_response()
            }
            p if p.ends_with("/leak/refresh") => {
                axum::Json(json!({ "t": { "u": self.refresh() } })).into_response()
            }
            p if p.ends_with("/identifiers") => {
                axum::Json(json!({ "id": "rt_short", "jwt": "a.b.c" })).into_response()
            }
            _ => "ok".into_response(),
        }
    }

    /// A JSON answer, gzip-compressed when the options say so.
    fn json(&self, value: Value) -> Response {
        if self.opts.gzip {
            return gzipped("application/json", &value.to_string());
        }
        axum::Json(value).into_response()
    }

    fn token(&self, seen: &Seen) -> Response {
        let body: Value = serde_json::from_str(&seen.body)
            .unwrap_or_else(|_| serde_json::to_value(form_fields(&seen.body)).unwrap());
        let invalid_grant = (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "invalid_grant" })),
        )
            .into_response();
        match body["grant_type"].as_str() {
            Some("authorization_code") => {
                if body["code"] != REAL_CODE && body["code"] != REAL_DEVICE_CODE {
                    return invalid_grant;
                }
                let mut answer = self.exchange_answer();
                if let Some(Value::Object(extra)) = &self.opts.exchange_extra {
                    answer.as_object_mut().unwrap().extend(extra.clone());
                }
                self.json(answer)
            }
            Some("refresh_token") if body["refresh_token"] == self.refresh().as_str() => {
                let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                self.rotate(n);
                let mut answer = json!({
                    "access_token": self.access(),
                    "refresh_token": self.refresh(),
                });
                if self.service == ServiceId::Anthropic {
                    answer["expires_in"] = 28_800.into();
                    answer["scope"] = "user:inference user:profile".into();
                }
                if let Some(plan) = &*self.refreshed_plan.lock().unwrap() {
                    answer["id_token"] = Self::id_token(plan).into();
                }
                if let Some(Value::Object(extra)) = &*self.refresh_extra.lock().unwrap() {
                    answer.as_object_mut().unwrap().extend(extra.clone());
                }
                self.json(answer)
            }
            Some("refresh_token") if self.service == ServiceId::Openai => (
                StatusCode::UNAUTHORIZED,
                axum::Json(json!({ "error": { "code": "refresh_token_invalidated" } })),
            )
                .into_response(),
            _ => invalid_grant,
        }
    }

    fn exchange_answer(&self) -> Value {
        match self.service {
            ServiceId::Anthropic => {
                let mut answer = json!({
                    "token_type": "Bearer",
                    "access_token": self.access(),
                    "refresh_token": self.refresh(),
                    "expires_in": self.opts.expires_in,
                    "refresh_token_expires_in": 7_776_000,
                    "scope": CLAUDE_SCOPE,
                    "token_uuid": "4f0b8a3e-2c1d-4e5f-9a6b-7c8d9e0f1a2b",
                    "organization": { "uuid": "org-1", "name": "Org" },
                });
                if self.opts.account {
                    answer["account"] =
                        json!({ "uuid": "acct-1", "email_address": "a@example.com" });
                }
                answer
            }
            ServiceId::Openai => json!({
                "id_token": Self::id_token("plus"),
                "access_token": self.access(),
                "refresh_token": self.refresh(),
                "expires_in": self.opts.expires_in,
            }),
        }
    }
}

fn token_path(service: ServiceId) -> &'static str {
    match service {
        ServiceId::Anthropic => "/v1/oauth/token",
        ServiceId::Openai => "/oauth/token",
    }
}

fn revoke_path(service: ServiceId) -> &'static str {
    match service {
        ServiceId::Anthropic => "/v1/oauth/token/revoke",
        ServiceId::Openai => "/oauth/revoke",
    }
}

/// The OAuth client a sign-in of `service` uses by default.
pub fn client_id(service: ServiceId) -> &'static str {
    match service {
        ServiceId::Anthropic => CLAUDE_CLIENT_ID,
        ServiceId::Openai => CODEX_CLIENT_ID,
    }
}

/// The loopback callback port and `redirect_uri` of a sign-in.
fn loopback(service: ServiceId) -> (u16, &'static str) {
    match service {
        ServiceId::Anthropic => (40000, "http://localhost:40000/callback"),
        ServiceId::Openai => (1455, "http://127.0.0.1:1455/auth/callback"),
    }
}

/// Everything one test needs, built before the runtime starts.
pub struct Setup {
    service_id: ServiceId,
    pub fake: Arc<FakeProvider>,
    token: FakeUpstream,
    api: Option<FakeUpstream>,
    home: StoreHome,
    pub store: Arc<TokenStore>,
    pub codes: PendingCodes,
    pub service: Rc<dyn Interceptor>,
}

impl Setup {
    pub fn new(service_id: ServiceId, opts: Options) -> Self {
        let alpn: &[&[u8]] = &[b"h2", b"http/1.1"];
        let token = FakeUpstream::bind(alpn);
        let api = (opts.api != ApiHost::TokenHost || service_id == ServiceId::Openai)
            .then(|| FakeUpstream::bind(alpn));
        let api_endpoint = api.as_ref().unwrap_or(&token).endpoint();
        let (home, store) = test_store();
        let codes = PendingCodes::default();
        let service: Rc<dyn Interceptor> = match service_id {
            ServiceId::Anthropic => Rc::new(anthropic::Anthropic::new(
                anthropic::Endpoints {
                    token: token.endpoint(),
                    api: api_endpoint,
                },
                store.clone(),
                token.client_tls(),
                codes.clone(),
            )),
            ServiceId::Openai => Rc::new(openai::Openai::new(
                openai::Endpoints {
                    auth: token.endpoint(),
                    chatgpt: api_endpoint,
                },
                store.clone(),
                token.client_tls(),
                codes.clone(),
            )),
        };
        Self {
            service_id,
            fake: FakeProvider::new(service_id, opts),
            token,
            api,
            home,
            store,
            codes,
            service,
        }
    }

    pub fn token_port(&self) -> u16 {
        self.token.port()
    }

    /// The network config: no allow rule (the service allows its hosts),
    /// deny-by-default, trusting the fake upstreams.
    pub fn config(&self) -> TestNetworkConfig {
        let mut trust_cas = vec![self.token.ca_pem()];
        trust_cas.extend(self.api.as_ref().map(FakeUpstream::ca_pem));
        TestNetworkConfig {
            allowed_hosts: vec![],
            trust_cas,
            interceptors: vec![self.service.clone()],
            ..Default::default()
        }
    }

    /// Start the fake upstreams (inside the runtime), for a guest on
    /// `proxy` that trusts the sandbox CA `mitm`.
    pub fn serve(self, proxy: Proxy, mitm: String) -> Running {
        let token_port = self.token.port();
        let api_port = self.api.as_ref().map_or(token_port, FakeUpstream::port);
        self.token.serve(self.fake.app());
        let ws_read = Arc::new(Mutex::new(Vec::new()));
        if let Some(api) = self.api {
            if self.fake.opts.api == ApiHost::WebSocket {
                api.serve_upgrade_echo(ws_read.clone());
            } else {
                api.serve(self.fake.app());
            }
        }
        Running {
            service: self.service_id,
            proxy,
            mitm,
            token_port,
            api_port,
            fake: self.fake,
            store: self.store,
            codes: self.codes,
            home: self.home,
            ws_read,
        }
    }

    /// Run `f` on a network of [`Self::config`].
    pub fn run<F, Fut>(self, f: F)
    where
        F: FnOnce(Running) -> Fut,
        Fut: Future<Output = ()>,
    {
        let cfg = self.config();
        self.run_with(cfg, f);
    }

    /// Run `f` on a network of `cfg`.
    pub fn run_with<F, Fut>(self, cfg: TestNetworkConfig, f: F)
    where
        F: FnOnce(Running) -> Fut,
        Fut: Future<Output = ()>,
    {
        run_with_config(cfg, |proxy, _log, mitm| async move {
            f(self.serve(proxy, mitm)).await;
        });
    }
}

/// A started setup and the guest's side of it.
pub struct Running {
    pub service: ServiceId,
    pub proxy: Proxy,
    pub mitm: String,
    /// The token host (and the API, unless it has its own).
    pub token_port: u16,
    pub api_port: u16,
    pub fake: Arc<FakeProvider>,
    pub store: Arc<TokenStore>,
    pub codes: PendingCodes,
    home: StoreHome,
    /// Every byte the WebSocket endpoint read.
    pub ws_read: Arc<Mutex<Vec<u8>>>,
}

impl Running {
    /// One guest request to `127.0.0.1:port` over h1 or h2.
    pub async fn request(&self, port: u16, h2: bool, req: Request<Full<Bytes>>) -> GuestResponse {
        guest_request(&self.proxy, &self.mitm, port, h2, req).await
    }

    /// One guest request to the token host over h1.
    pub async fn on_token_host(&self, req: Request<Full<Bytes>>) -> GuestResponse {
        self.request(self.token_port, false, req).await
    }

    /// One guest request to the API host over h1.
    pub async fn on_api(&self, req: Request<Full<Bytes>>) -> GuestResponse {
        self.request(self.api_port, false, req).await
    }

    /// A GET of `path` on the API host with `Authorization: Bearer token`.
    pub async fn api_get(&self, path: &str, token: &str) -> GuestResponse {
        self.on_api(super::upstream::get_with_bearer(path, token))
            .await
    }

    /// A POST of JSON `body` to the token endpoint.
    pub async fn post_token(&self, body: &Value) -> GuestResponse {
        self.on_token_host(post(
            token_path(self.service),
            "application/json",
            &body.to_string(),
        ))
        .await
    }

    /// A POST of JSON `body` to the revoke endpoint.
    pub async fn post_revoke(&self, body: &Value) -> GuestResponse {
        self.on_token_host(post(
            revoke_path(self.service),
            "application/json",
            &body.to_string(),
        ))
        .await
    }

    /// Anthropic's `create_api_key` with the access token `access`.
    pub async fn create_api_key(&self, access: &str) -> GuestResponse {
        let mut req = post(
            "/api/oauth/claude_cli/create_api_key",
            "application/json",
            "",
        );
        req.headers_mut()
            .insert("authorization", format!("Bearer {access}").parse().unwrap());
        self.on_token_host(req).await
    }

    /// The guest's code exchange as the agent sends it: JSON for Claude
    /// Code, a form for Codex.
    pub async fn exchange_with(
        &self,
        client_id: &str,
        code: &str,
        redirect_uri: &str,
    ) -> GuestResponse {
        let req = match self.service {
            ServiceId::Anthropic => post(
                token_path(self.service),
                "application/json",
                &self
                    .exchange_body(client_id, code, redirect_uri)
                    .to_string(),
            ),
            ServiceId::Openai => post(
                token_path(self.service),
                "application/x-www-form-urlencoded",
                &url::form_urlencoded::Serializer::new(String::new())
                    .append_pair("grant_type", "authorization_code")
                    .append_pair("code", code)
                    .append_pair("redirect_uri", redirect_uri)
                    .append_pair("client_id", client_id)
                    .append_pair("code_verifier", VERIFIER)
                    .finish(),
            ),
        };
        self.on_token_host(req).await
    }

    /// The fields of the guest's code exchange.
    pub fn exchange_body(&self, client_id: &str, code: &str, redirect_uri: &str) -> Value {
        let mut body = json!({
            "grant_type": "authorization_code",
            "code": code,
            "redirect_uri": redirect_uri,
            "client_id": client_id,
            "code_verifier": VERIFIER,
        });
        if self.service == ServiceId::Anthropic {
            body["state"] = "s".into();
        }
        body
    }

    /// The code exchange of the default client on the loopback callback.
    pub async fn exchange(&self, code: &str) -> GuestResponse {
        self.exchange_with(client_id(self.service), code, self.loopback_redirect())
            .await
    }

    pub fn loopback_redirect(&self) -> &'static str {
        loopback(self.service).1
    }

    /// A surrogate code of the service's loopback callback for the real
    /// code: what the callback forward hands the guest.
    pub fn issue_code(&self) -> String {
        let port = loopback(self.service).0;
        self.codes
            .issue(REAL_CODE, self.service, Channel::Callback(port))
            .unwrap()
    }

    /// A sign-in: the exchange of an issued surrogate code. The answer
    /// carries no real token.
    pub async fn sign_in(&self) -> Value {
        let resp = self.exchange(&self.issue_code()).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(
            !resp.body.contains("REAL"),
            "a real token reached the guest: {}",
            resp.body
        );
        resp.json()
    }

    /// The requests the provider got.
    pub fn upstream_requests(&self) -> usize {
        self.fake.seen.all().len()
    }

    /// The last request the provider got.
    pub fn last_seen(&self) -> Seen {
        self.fake.seen.last()
    }

    /// The JSON body of the last request the provider got.
    pub fn last_body(&self) -> Value {
        serde_json::from_str(&self.last_seen().body).unwrap()
    }

    /// The bodies of the revokes the provider got.
    pub fn revokes(&self) -> Vec<Value> {
        self.fake
            .seen
            .all()
            .into_iter()
            .filter(|s| s.path == revoke_path(self.service))
            .map(|s| serde_json::from_str(&s.body).unwrap())
            .collect()
    }

    /// The requests to the token endpoint the provider got.
    pub fn token_requests(&self) -> usize {
        self.fake
            .seen
            .all()
            .iter()
            .filter(|s| s.path == token_path(self.service))
            .count()
    }

    /// Wait (a little) until the provider got `n` revokes.
    pub async fn wait_for_revokes(&self, n: usize) {
        for _ in 0..50 {
            if self.revokes().len() >= n {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    /// The grants in the store.
    pub async fn grants(&self) -> Vec<GrantSummary> {
        list_grants(&self.home.db).await.unwrap()
    }
}
