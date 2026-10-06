//! The `anthropic` service end to end: a guest signs in and calls the API
//! through the proxy; a fake provider upstream records what it receives.

use std::io::Write as _;
use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use serde_json::{Value, json};

use super::fake_provider::*;
use super::helpers::*;
use crate::network::interceptor::Interceptor;
use crate::services::ServiceId;
use crate::services::anthropic::{Anthropic, Endpoints};
use crate::services::auth_codes::{Channel, PendingCodes};
use crate::services::store::{
    GrantSecrets, NewGrant, SurrogateKind, Surrogates, TokenStore, list_grants, now_ms,
};

const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const LOOPBACK: &str = "http://localhost:40000/callback";
const MANUAL: &str = "https://platform.claude.com/oauth/code/callback";
/// The code the fake provider issued: what the browser brings back.
const REAL_CODE: &str = "real-code";
const REAL_API_KEY: &str = "sk-ant-api03-REAL-KEY";
const SCOPE: &str = "org:create_api_key user:inference user:profile";

/// The guest's code exchange body.
fn code_exchange(code: &str, redirect_uri: &str) -> String {
    json!({
        "grant_type": "authorization_code",
        "code": code,
        "redirect_uri": redirect_uri,
        "client_id": CLIENT_ID,
        "code_verifier": "v",
        "state": "s",
    })
    .to_string()
}

/// How the fake provider behaves.
#[derive(Clone)]
struct Options {
    /// `expires_in` of the code exchange.
    expires_in: i64,
    /// `expires_in` of a refresh.
    refresh_expires_in: i64,
    /// The answer to every refresh, instead of new tokens.
    refresh_error: Option<(u16, Value)>,
    /// How long a refresh takes.
    refresh_delay: Duration,
    /// The exchange issues a refresh token (`/login`; not `setup-token`).
    issue_refresh: bool,
    /// The exchange names the account.
    account: bool,
    /// Token answers are gzip-compressed.
    gzip: bool,
    /// The answer status of a revoke.
    revoke_status: u16,
    /// How long a revoke takes.
    revoke_delay: Duration,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            expires_in: 3600,
            refresh_expires_in: 28_800,
            refresh_error: None,
            refresh_delay: Duration::ZERO,
            issue_refresh: true,
            account: true,
            gzip: false,
            revoke_status: 200,
            revoke_delay: Duration::ZERO,
        }
    }
}

/// The provider's side: the tokens it accepts right now.
struct Fake {
    seen: SeenLog,
    opts: Options,
    access: Mutex<String>,
    refresh: Mutex<String>,
    /// Successful refreshes.
    refreshes: AtomicUsize,
    /// Refresh requests, failed or not.
    refresh_calls: AtomicUsize,
}

impl Fake {
    fn new(opts: Options) -> Arc<Self> {
        Arc::new(Self {
            seen: SeenLog::default(),
            opts,
            access: Mutex::new("sk-ant-oat01-REAL-ACCESS-0".into()),
            refresh: Mutex::new("sk-ant-ort01-REAL-REFRESH-0".into()),
            refreshes: AtomicUsize::new(0),
            refresh_calls: AtomicUsize::new(0),
        })
    }

    fn access(&self) -> String {
        self.access.lock().unwrap().clone()
    }

    fn app(self: &Arc<Self>) -> Router {
        let fake = self.clone();
        Router::new().fallback(move |req: axum::extract::Request| {
            let fake = fake.clone();
            async move { fake.handle(req).await }
        })
    }

    async fn handle(&self, req: axum::extract::Request) -> axum::response::Response {
        let seen = self.seen.record(req).await;
        let bearer = seen
            .header("authorization")
            .and_then(|v| v.strip_prefix("Bearer "))
            .map(String::from);
        let authorized = bearer.as_deref() == Some(self.access().as_str());
        match (seen.method.as_str(), seen.path.as_str()) {
            ("POST", "/v1/oauth/token") => self.token(&seen).await,
            ("POST", "/v1/oauth/token/revoke") => {
                tokio::time::sleep(self.opts.revoke_delay).await;
                let status = StatusCode::from_u16(self.opts.revoke_status).unwrap();
                (status, axum::Json(json!({}))).into_response()
            }
            ("POST", "/api/oauth/claude_cli/create_api_key") if authorized => {
                self.json(json!({ "raw_key": REAL_API_KEY, "name": "claude-code" }))
            }
            ("GET", "/page/json") => axum::Json(json!({ "access_token": "x" })).into_response(),
            ("GET", "/page/html") => (
                [("content-type", "text/html")],
                "<p>access_token sk-ant-oat01-x</p>",
            )
                .into_response(),
            ("GET", "/page/big") => axum::Json(json!({
                "access_token": "x",
                "pad": "a".repeat(70 * 1024),
            }))
            .into_response(),
            _ if authorized || seen.header("x-api-key") == Some(REAL_API_KEY) => {
                "ok".into_response()
            }
            _ => (StatusCode::UNAUTHORIZED, "no").into_response(),
        }
    }

    /// A JSON answer, gzip-compressed when the options say so.
    fn json(&self, value: Value) -> axum::response::Response {
        if !self.opts.gzip {
            return axum::Json(value).into_response();
        }
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        gz.write_all(value.to_string().as_bytes()).unwrap();
        (
            [
                ("content-type", "application/json"),
                ("content-encoding", "gzip"),
            ],
            gz.finish().unwrap(),
        )
            .into_response()
    }

    async fn token(&self, seen: &Seen) -> axum::response::Response {
        let body: Value = serde_json::from_str(&seen.body).unwrap();
        let bad = (
            StatusCode::BAD_REQUEST,
            axum::Json(json!({ "error": "invalid_grant" })),
        );
        match body["grant_type"].as_str() {
            Some("authorization_code") => {
                if body["code"] != REAL_CODE {
                    return bad.into_response();
                }
                let mut answer = json!({
                    "token_type": "Bearer",
                    "access_token": self.access(),
                    "expires_in": self.opts.expires_in,
                    "refresh_token_expires_in": 7_776_000,
                    "scope": SCOPE,
                    "organization": { "uuid": "org-1", "name": "Org" },
                });
                if self.opts.issue_refresh {
                    answer["refresh_token"] = (*self.refresh.lock().unwrap()).clone().into();
                }
                if self.opts.account {
                    answer["account"] =
                        json!({ "uuid": "acct-1", "email_address": "a@example.com" });
                }
                self.json(answer)
            }
            Some("refresh_token") => {
                self.refresh_calls.fetch_add(1, Ordering::SeqCst);
                tokio::time::sleep(self.opts.refresh_delay).await;
                if let Some((status, error)) = &self.opts.refresh_error {
                    return (
                        StatusCode::from_u16(*status).unwrap(),
                        axum::Json(error.clone()),
                    )
                        .into_response();
                }
                let current = self.refresh.lock().unwrap().clone();
                if body["refresh_token"] != current.as_str() {
                    return bad.into_response();
                }
                let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
                *self.access.lock().unwrap() = format!("sk-ant-oat01-REAL-ACCESS-{n}");
                *self.refresh.lock().unwrap() = format!("sk-ant-ort01-REAL-REFRESH-{n}");
                axum::Json(json!({
                    "access_token": self.access(),
                    "refresh_token": *self.refresh.lock().unwrap(),
                    "expires_in": self.opts.refresh_expires_in,
                    "scope": "user:inference user:profile",
                }))
                .into_response()
            }
            _ => bad.into_response(),
        }
    }
}

/// Everything one test needs, built before the runtime starts. The token
/// and API endpoints are one upstream, unless split.
struct Setup {
    upstream: FakeUpstream,
    api: Option<FakeUpstream>,
    fake: Arc<Fake>,
    home: StoreHome,
    store: Arc<TokenStore>,
    codes: PendingCodes,
    service: Rc<dyn Interceptor>,
}

fn setup(opts: Options) -> Setup {
    setup_with(opts, &[b"http/1.1"], false)
}

fn setup_with(opts: Options, alpn: &[&[u8]], split: bool) -> Setup {
    let upstream = FakeUpstream::bind(alpn);
    let api = split.then(|| FakeUpstream::bind(alpn));
    let (home, store) = test_store();
    let endpoints = Endpoints {
        token: upstream.endpoint(),
        api: api.as_ref().unwrap_or(&upstream).endpoint(),
    };
    let codes = PendingCodes::default();
    let service: Rc<dyn Interceptor> = Rc::new(Anthropic::new(
        endpoints,
        store.clone(),
        upstream.client_tls(),
        codes.clone(),
    ));
    Setup {
        upstream,
        api,
        fake: Fake::new(opts),
        home,
        store,
        codes,
        service,
    }
}

/// A started setup.
struct Running {
    /// The token endpoint (and the API, unless split).
    port: u16,
    /// The API endpoint.
    api_port: u16,
    fake: Arc<Fake>,
    store: Arc<TokenStore>,
    codes: PendingCodes,
    home: StoreHome,
}

impl Setup {
    /// The network config: no allow rule at all (the service allows its
    /// host), deny-by-default.
    fn config(&self) -> TestNetworkConfig {
        let mut trust_cas = vec![self.upstream.ca_pem()];
        trust_cas.extend(self.api.as_ref().map(FakeUpstream::ca_pem));
        TestNetworkConfig {
            allowed_hosts: vec![],
            trust_cas,
            interceptors: vec![self.service.clone()],
            ..Default::default()
        }
    }

    /// Start the fake upstreams (inside the runtime).
    fn serve(self) -> Running {
        let port = self.upstream.port();
        let api_port = self.api.as_ref().map_or(port, FakeUpstream::port);
        self.upstream.serve(self.fake.app());
        if let Some(api) = self.api {
            api.serve(self.fake.app());
        }
        Running {
            port,
            api_port,
            fake: self.fake,
            store: self.store,
            codes: self.codes,
            home: self.home,
        }
    }
}

type Proxy = airlock_common::network_capnp::network_proxy::Client;

/// A POST of JSON to the token endpoint.
async fn token_request(
    proxy: &Proxy,
    mitm: &str,
    r: &Running,
    path: &str,
    body: &str,
) -> GuestResponse {
    guest_request(
        proxy,
        mitm,
        r.port,
        false,
        post(path, "application/json", body),
    )
    .await
}

/// The guest's code exchange, with the surrogate code the callback
/// forward handed it; returns the answer.
async fn sign_in(proxy: &Proxy, mitm: &str, r: &Running) -> Value {
    let code = r
        .codes
        .issue(REAL_CODE, ServiceId::Anthropic, Channel::Callback(40000))
        .unwrap();
    let resp = token_request(
        proxy,
        mitm,
        r,
        "/v1/oauth/token",
        &code_exchange(&code, LOOPBACK),
    )
    .await;
    assert_eq!(resp.status, 200, "{}", resp.body);
    assert!(
        !resp.body.contains("REAL"),
        "a real token reached the guest: {}",
        resp.body
    );
    resp.json()
}

/// An API request with the access surrogate.
async fn api_get(proxy: &Proxy, mitm: &str, r: &Running, access: &str) -> GuestResponse {
    guest_request(
        proxy,
        mitm,
        r.api_port,
        false,
        get_with_bearer("/v1/messages", access),
    )
    .await
}

fn refresh_body(refresh: &Value) -> String {
    json!({
        "grant_type": "refresh_token",
        "refresh_token": refresh,
        "client_id": CLIENT_ID,
        "scope": "user:inference",
    })
    .to_string()
}

#[test]
fn a_code_exchange_gives_the_guest_surrogates_and_stores_the_real_tokens() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        let refresh = answer["refresh_token"].as_str().unwrap();
        assert!(access.starts_with("sk-ant-oat01-airlock-"), "{access}");
        assert!(refresh.starts_with("sk-ant-ort01-airlock-"), "{refresh}");
        assert_eq!(answer["expires_in"], 3600, "the upstream's own expires_in");
        assert_eq!(answer["scope"], SCOPE);
        assert_eq!(answer["account"]["email_address"], "a@example.com");
        assert_eq!(answer["organization"]["name"], "Org");
        assert!(answer.get("refresh_token_expires_in").is_none());

        // The exchange went upstream with the real code, re-serialized.
        let exchange = r.fake.seen.last();
        assert_eq!(exchange.path, "/v1/oauth/token");
        let mut want: Value = serde_json::from_str(&code_exchange(REAL_CODE, LOOPBACK)).unwrap();
        want["code"] = REAL_CODE.into();
        assert_eq!(serde_json::from_str::<Value>(&exchange.body).unwrap(), want);
        assert_eq!(exchange.body, want.to_string());
        let grant = r
            .store
            .find_by_surrogate(ServiceId::Anthropic, SurrogateKind::Access, access)
            .await
            .unwrap()
            .expect("the grant is stored");
        assert_eq!(grant.secrets.access_token, "sk-ant-oat01-REAL-ACCESS-0");
        assert_eq!(
            grant.secrets.refresh_token.as_deref(),
            Some("sk-ant-ort01-REAL-REFRESH-0")
        );
        assert_eq!(
            grant.secrets.scopes,
            ["org:create_api_key", "user:inference", "user:profile"]
        );
    });
}

/// The real code never reaches the guest, so an exchange must bring a
/// surrogate code: any other code is refused locally. The manual sign-in
/// is the one exception.
#[test]
fn an_exchange_needs_a_surrogate_code_except_for_the_manual_sign_in() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        for code in [REAL_CODE, "airlock-code-unknown"] {
            let resp = token_request(
                &proxy,
                &mitm,
                &r,
                "/v1/oauth/token",
                &code_exchange(code, LOOPBACK),
            )
            .await;
            assert_eq!(resp.status, 400, "{code}");
            assert_eq!(resp.json()["error"], "invalid_grant");
        }
        assert!(r.fake.seen.all().is_empty(), "nothing went upstream");

        // A surrogate works once.
        let code = r
            .codes
            .issue(REAL_CODE, ServiceId::Anthropic, Channel::Callback(40000))
            .unwrap();
        let exchange = code_exchange(&code, LOOPBACK);
        let resp = token_request(&proxy, &mitm, &r, "/v1/oauth/token", &exchange).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        let resp = token_request(&proxy, &mitm, &r, "/v1/oauth/token", &exchange).await;
        assert_eq!(resp.status, 400);
        assert_eq!(r.fake.seen.all().len(), 1);

        // The manual sign-in brings the real code itself.
        let resp = token_request(
            &proxy,
            &mitm,
            &r,
            "/v1/oauth/token",
            &code_exchange(REAL_CODE, MANUAL),
        )
        .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
        assert_eq!(r.fake.seen.all().len(), 2);
    });
}

#[test]
fn api_requests_carry_the_real_token_upstream_over_h1_and_h2() {
    let s = setup_with(Options::default(), &[b"h2", b"http/1.1"], false);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        for h2 in [false, true] {
            let resp = guest_request(
                &proxy,
                &mitm,
                r.port,
                h2,
                get_with_bearer("/v1/messages", access),
            )
            .await;
            assert_eq!(resp.status, 200, "h2={h2}: {}", resp.body);
            assert_eq!(
                r.fake.seen.last().header("authorization"),
                Some("Bearer sk-ant-oat01-REAL-ACCESS-0"),
                "h2={h2}"
            );
        }
    });
}

/// Tokens are swapped on the API host only; the token host gets no bearer
/// swap, and the token paths are token endpoints on the token host only.
#[test]
fn surrogates_are_swapped_on_the_api_host_only() {
    let s = setup_with(Options::default(), &[b"http/1.1"], true);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        let resp = api_get(&proxy, &mitm, &r, access).await;
        assert_eq!(resp.status, 200, "{}", resp.body);

        let resp = guest_request(
            &proxy,
            &mitm,
            r.port,
            false,
            get_with_bearer("/v1/messages", access),
        )
        .await;
        assert_eq!(resp.status, 401);
        assert_eq!(
            r.fake.seen.last().header("authorization"),
            Some(format!("Bearer {access}").as_str())
        );

        // A refresh on the API host is no token request: it goes
        // upstream as it is.
        let resp = guest_request(
            &proxy,
            &mitm,
            r.api_port,
            false,
            post(
                "/v1/oauth/token",
                "application/json",
                &refresh_body(&answer["refresh_token"]),
            ),
        )
        .await;
        assert_eq!(resp.status, 400);
        assert!(r.fake.seen.last().body.contains("sk-ant-ort01-airlock-"));
    });
}

/// Token requests are parsed strictly and refused locally when they do
/// not fit: they never reach the provider.
#[test]
fn malformed_token_requests_are_refused_locally() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let before = r.fake.seen.all().len();
        let refresh = refresh_body(&answer["refresh_token"]);
        let duplicate = refresh.replacen('{', r#"{"grant_type":"authorization_code","#, 1);
        for (path, content_type, body, error) in [
            (
                "/v1/oauth/token?x=1",
                "application/json",
                refresh.as_str(),
                "invalid_request",
            ),
            (
                "/v1/oauth/token",
                "application/json",
                duplicate.as_str(),
                "invalid_request",
            ),
            (
                "/v1/oauth/token",
                "text/plain",
                refresh.as_str(),
                "invalid_request",
            ),
            (
                "/v1/oauth/token",
                "application/json",
                "[1]",
                "invalid_request",
            ),
            (
                "/v1/oauth/token",
                "application/json",
                r#"{"grant_type":"client_credentials","client_id":"x"}"#,
                "unsupported_grant_type",
            ),
            (
                "/v1/oauth/token",
                "application/x-www-form-urlencoded",
                "grant_type=password&username=u",
                "unsupported_grant_type",
            ),
        ] {
            let resp =
                guest_request(&proxy, &mitm, r.port, false, post(path, content_type, body)).await;
            assert_eq!(resp.status, 400, "{path} {body}");
            assert_eq!(resp.json()["error"], error, "{path} {body}");
        }
        assert_eq!(r.fake.seen.all().len(), before, "nothing went upstream");
    });
}

/// Strict credentials on the API host: a token that is no surrogate
/// airlock knows and no injected secret never goes upstream. An unknown
/// surrogate means a sign-out; anything else a credential from elsewhere
/// (another sandbox may have planted it in the shared credential file).
#[test]
fn unknown_credentials_are_refused_and_their_refresh_too() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        sign_in(&proxy, &mitm, &r).await;
        let before = r.fake.seen.all().len();
        for (unknown, says) in [
            ("sk-ant-oat01-airlock-unknown", "sign in again"),
            (
                "sk-ant-oat01-REAL-ACCESS-0",
                "[network.services] anthropic enabled",
            ),
            ("sk-ant-oat01-unknown", "masked [env] secret with inject"),
        ] {
            let resp = api_get(&proxy, &mitm, &r, unknown).await;
            assert_eq!(resp.status, 401, "{unknown}");
            assert!(resp.body.contains(says), "{unknown}: {}", resp.body);
        }
        for (name, value) in [
            ("x-api-key", REAL_API_KEY),
            ("x-api-key", "sk-ant-api03-airlock-unknown"),
            ("authorization", "Basic dXNlcjpwYXNz"),
        ] {
            let req = hyper::Request::get("/v1/messages")
                .header(name, value)
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .unwrap();
            let resp = guest_request(&proxy, &mitm, r.api_port, false, req).await;
            assert_eq!(resp.status, 401, "{value}: {}", resp.body);
        }
        assert_eq!(r.fake.seen.all().len(), before, "nothing went upstream");

        let before = r.fake.seen.all().len();
        for unknown in ["sk-ant-ort01-unknown", "sk-ant-ort01-airlock-unknown"] {
            let resp = token_request(
                &proxy,
                &mitm,
                &r,
                "/v1/oauth/token",
                &refresh_body(&unknown.into()),
            )
            .await;
            assert_eq!(resp.status, 400);
            assert_eq!(resp.json()["error"], "invalid_grant");
        }
        assert_eq!(r.fake.seen.all().len(), before, "answered locally");
    });
}

/// Claude's `/logout` revokes its refresh token: the grant is deleted and
/// its real token revoked upstream, for every sandbox.
#[test]
fn a_revoke_deletes_the_grant_and_revokes_the_real_token_upstream() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let revoke = json!({
            "token": answer["refresh_token"],
            "token_type_hint": "refresh_token",
            "client_id": CLIENT_ID,
        });
        let resp = token_request(
            &proxy,
            &mitm,
            &r,
            "/v1/oauth/token/revoke",
            &revoke.to_string(),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.json(), json!({}));
        let upstream = r.fake.seen.last();
        assert_eq!(upstream.path, "/v1/oauth/token/revoke");
        assert_eq!(
            serde_json::from_str::<Value>(&upstream.body).unwrap(),
            json!({
                "token": "sk-ant-ort01-REAL-REFRESH-0",
                "token_type_hint": "refresh_token",
                "client_id": CLIENT_ID,
            })
        );
        assert!(list_grants(&r.home.db).await.unwrap().is_empty());

        // The surrogates of the deleted grant are unknown now: refused
        // locally, never sent upstream.
        let access = answer["access_token"].as_str().unwrap();
        let resp = api_get(&proxy, &mitm, &r, access).await;
        assert_eq!(resp.status, 401);
        assert!(resp.body.contains("sign in again"), "{}", resp.body);
        assert_eq!(r.fake.seen.last().path, "/v1/oauth/token/revoke");
        let resp = token_request(
            &proxy,
            &mitm,
            &r,
            "/v1/oauth/token",
            &refresh_body(&answer["refresh_token"]),
        )
        .await;
        assert_eq!(resp.status, 400);
    });
}

/// Either surrogate signs out the whole grant: Anthropic revokes refresh
/// tokens (Claude Code never revokes an access token), so the access
/// surrogate revokes the real refresh token.
#[test]
fn a_revoke_of_the_access_surrogate_revokes_the_real_refresh_token() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let revoke = json!({ "token": answer["access_token"], "client_id": CLIENT_ID });
        let resp = token_request(
            &proxy,
            &mitm,
            &r,
            "/v1/oauth/token/revoke",
            &revoke.to_string(),
        )
        .await;
        assert_eq!(resp.status, 200);
        let revokes: Vec<Value> = r
            .fake
            .seen
            .all()
            .into_iter()
            .filter(|s| s.path == "/v1/oauth/token/revoke")
            .map(|s| serde_json::from_str(&s.body).unwrap())
            .collect();
        assert_eq!(revokes.len(), 1, "{revokes:?}");
        assert_eq!(revokes[0]["token"], "sk-ant-ort01-REAL-REFRESH-0");
        assert_eq!(revokes[0]["token_type_hint"], "refresh_token");
        assert!(list_grants(&r.home.db).await.unwrap().is_empty());
    });
}

/// Unknown tokens, and API-key surrogates, are no sign-out: answered
/// locally, nothing deleted, nothing sent upstream.
#[test]
fn a_revoke_of_an_unknown_token_stays_local() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let mut req = post(
            "/api/oauth/claude_cli/create_api_key",
            "application/json",
            "",
        );
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {}", answer["access_token"].as_str().unwrap())
                .parse()
                .unwrap(),
        );
        let key = guest_request(&proxy, &mitm, r.port, false, req)
            .await
            .json()["raw_key"]
            .clone();
        let before = r.fake.seen.all().len();
        for token in [
            json!("sk-ant-ort01-airlock-unknown"),
            json!("sk-ant-ort01-REAL-REFRESH-0"),
            key,
            json!(42),
        ] {
            let resp = token_request(
                &proxy,
                &mitm,
                &r,
                "/v1/oauth/token/revoke",
                &json!({ "token": token }).to_string(),
            )
            .await;
            assert_eq!(resp.status, 200, "{token}");
        }
        assert_eq!(r.fake.seen.all().len(), before);
        assert_eq!(list_grants(&r.home.db).await.unwrap().len(), 1);
    });
}

#[test]
fn a_failed_upstream_revoke_still_deletes_the_grant() {
    let s = setup(Options {
        revoke_status: 503,
        ..Options::default()
    });
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let revoke = json!({ "token": answer["refresh_token"] });
        let resp = token_request(
            &proxy,
            &mitm,
            &r,
            "/v1/oauth/token/revoke",
            &revoke.to_string(),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(r.fake.seen.last().path, "/v1/oauth/token/revoke");
        assert!(list_grants(&r.home.db).await.unwrap().is_empty());
    });
}

/// A guest that gives up on its revoke does not stop the sign-out.
#[test]
fn a_dropped_revoke_still_completes() {
    let s = setup(Options {
        revoke_delay: Duration::from_millis(300),
        ..Options::default()
    });
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let revoke = json!({ "token": answer["refresh_token"] }).to_string();
        let dropped = tokio::time::timeout(
            Duration::from_millis(100),
            token_request(&proxy, &mitm, &r, "/v1/oauth/token/revoke", &revoke),
        )
        .await;
        assert!(dropped.is_err(), "the guest gave up first");
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(r.fake.seen.last().path, "/v1/oauth/token/revoke");
        assert!(list_grants(&r.home.db).await.unwrap().is_empty());
    });
}

/// Lua middleware and the monitor run before the service: they see the
/// surrogate, never the real token.
#[test]
fn middleware_and_monitor_see_only_the_surrogate() {
    let s = setup(Options::default());
    let mut cfg = s.config();
    cfg.allowed_hosts = vec!["127.0.0.1".into()];
    cfg.middleware_scripts = vec![(
        "log auth",
        r#"if req:header("authorization") then log(req:header("authorization")) end"#,
    )];
    block_on_local(async move {
        let (log, mitm, network) = build_network(cfg);
        let mut events = network.handle().events();
        let proxy = start_rpc(network);
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        let resp = api_get(&proxy, &mitm, &r, access).await;
        assert_eq!(resp.status, 200);
        assert_eq!(
            r.fake.seen.last().header("authorization"),
            Some("Bearer sk-ant-oat01-REAL-ACCESS-0")
        );
        assert_eq!(log.messages(), [format!("Bearer {access}")]);
        let mut auth_headers = Vec::new();
        while let Ok(ev) = events.try_recv() {
            if let airlock_monitor::NetworkEvent::Request(r) = ev {
                auth_headers.extend(
                    r.headers
                        .iter()
                        .filter(|(k, _)| k == "authorization")
                        .map(|(_, v)| v.clone()),
                );
            }
        }
        assert_eq!(auth_headers, [format!("Bearer {access}")]);
    });
}

/// Sign-ins that name no account never replace each other.
#[test]
fn sign_ins_without_an_account_are_kept_apart() {
    let s = setup(Options {
        account: false,
        ..Options::default()
    });
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        sign_in(&proxy, &mitm, &r).await;
        sign_in(&proxy, &mitm, &r).await;
        assert_eq!(list_grants(&r.home.db).await.unwrap().len(), 2);
    });
}

#[test]
fn a_created_api_key_reaches_the_guest_as_a_surrogate() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        let mut req = post(
            "/api/oauth/claude_cli/create_api_key",
            "application/json",
            "",
        );
        req.headers_mut()
            .insert("authorization", format!("Bearer {access}").parse().unwrap());
        let resp = guest_request(&proxy, &mitm, r.port, false, req).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
        let key = resp.json()["raw_key"].as_str().unwrap().to_string();
        assert!(key.starts_with("sk-ant-api03-airlock-"), "{key}");
        assert_eq!(resp.json()["name"], "claude-code");

        let req = hyper::Request::get("/v1/messages")
            .header("x-api-key", &key)
            .body(http_body_util::Full::new(bytes::Bytes::new()))
            .unwrap();
        let resp = guest_request(&proxy, &mitm, r.port, false, req).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(r.fake.seen.last().header("x-api-key"), Some(REAL_API_KEY));
    });
}

/// A token answer the proxy cannot read is never passed on.
#[test]
fn compressed_token_answers_are_refused() {
    let s = setup(Options {
        gzip: true,
        ..Options::default()
    });
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let code = r
            .codes
            .issue(REAL_CODE, ServiceId::Anthropic, Channel::Callback(40000))
            .unwrap();
        let resp = token_request(
            &proxy,
            &mitm,
            &r,
            "/v1/oauth/token",
            &code_exchange(&code, LOOPBACK),
        )
        .await;
        assert_eq!(resp.status, 502);
        assert_eq!(resp.json()["error"], "server_error");
        assert!(list_grants(&r.home.db).await.unwrap().is_empty());

        // A grant made without the proxy, for create_api_key.
        let grant = r
            .store
            .insert_grant(NewGrant {
                service: ServiceId::Anthropic,
                account_id: "acct".into(),
                account_label: None,
                secrets: GrantSecrets {
                    access_token: r.fake.access(),
                    access_expires_at: now_ms() + 3_600_000,
                    refresh_token: None,
                    id_token: None,
                    scopes: vec![],
                    surrogates: Surrogates {
                        access: "sk-ant-oat01-airlock-test".into(),
                        previous_access: vec![],
                        refresh: None,
                        id_token: None,
                    },
                    api_keys: vec![],
                },
            })
            .await
            .unwrap()
            .0;
        let mut req = post(
            "/api/oauth/claude_cli/create_api_key",
            "application/json",
            "",
        );
        req.headers_mut().insert(
            "authorization",
            format!("Bearer {}", grant.secrets.surrogates.access)
                .parse()
                .unwrap(),
        );
        let resp = guest_request(&proxy, &mitm, r.port, false, req).await;
        assert_eq!(resp.status, 502);
        assert!(!resp.body.contains("REAL"));
    });
}

/// Answers of the token host that carry a token are refused; other pages
/// pass.
#[test]
fn the_token_host_backstop_refuses_token_answers() {
    let s = setup_with(Options::default(), &[b"http/1.1"], true);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let get = |path: &str| {
            hyper::Request::get(path)
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .unwrap()
        };
        let resp = guest_request(&proxy, &mitm, r.port, false, get("/page/json")).await;
        assert_eq!(resp.status, 502);
        assert_eq!(resp.json()["error"], "server_error");
        for page in ["/page/html", "/page/big"] {
            let resp = guest_request(&proxy, &mitm, r.port, false, get(page)).await;
            assert_eq!(resp.status, 200, "{page}");
        }
        // The API host is not buffered or checked.
        let resp = guest_request(&proxy, &mitm, r.api_port, false, get("/page/json")).await;
        assert_eq!(resp.status, 200);
    });
}

/// On plain HTTP the service does nothing: the surrogate goes out as it
/// is.
#[test]
fn plain_http_to_an_owned_host_gets_no_swap() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_dir, store) = test_store();
    let endpoint = crate::network::target::Endpoint::new("127.0.0.1", port);
    let service: Rc<dyn Interceptor> = Rc::new(Anthropic::new(
        Endpoints {
            token: endpoint.clone(),
            api: endpoint,
        },
        store.clone(),
        Arc::new(tls_trusting("")),
        PendingCodes::default(),
    ));
    let cfg = TestNetworkConfig {
        interceptors: vec![service],
        ..Default::default()
    };
    run_with_config(cfg, |proxy, _log, _mitm| async move {
        let surrogate = "sk-ant-oat01-airlock-plain";
        store
            .insert_grant(NewGrant {
                service: ServiceId::Anthropic,
                account_id: "acct".into(),
                account_label: None,
                secrets: GrantSecrets {
                    access_token: "sk-ant-oat01-REAL".into(),
                    access_expires_at: now_ms() + 3_600_000,
                    refresh_token: None,
                    id_token: None,
                    scopes: vec![],
                    surrogates: Surrogates {
                        access: surrogate.into(),
                        previous_access: vec![],
                        refresh: None,
                        id_token: None,
                    },
                    api_keys: vec![],
                },
            })
            .await
            .unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let upstream = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let request = read_until_contains(&mut sock, "\r\n\r\n").await;
            tokio::io::AsyncWriteExt::write_all(
                &mut sock,
                b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
            )
            .await
            .unwrap();
            request
        });
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", port)
            .await
            .unwrap();
        let answer = conn
            .roundtrip(&format!(
                "GET /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {surrogate}\r\nConnection: close\r\n\r\n"
            ))
            .await;
        assert!(answer.contains("200"), "{answer}");
        let request = upstream.await.unwrap();
        assert!(request.contains(surrogate), "{request}");
        assert!(!request.contains("REAL"), "{request}");
    });
}

/// Requests that carry no surrogate do not touch the token store (its
/// databases are not even created). Their credentials are masked secrets
/// the inject rule puts in: the only real credentials an API request may
/// carry with the service on.
#[test]
fn requests_without_surrogates_do_not_touch_the_store() {
    let upstream = FakeUpstream::bind(&[b"http/1.1"]);
    let dir = tempfile::tempdir().unwrap();
    let db = crate::db::Db::open(&dir.path().join(crate::db::DIR)).unwrap();
    let store = Arc::new(TokenStore::new(db.clone(), &[42; 32]));
    let service: Rc<dyn Interceptor> = Rc::new(Anthropic::new(
        Endpoints {
            token: upstream.endpoint(),
            api: upstream.endpoint(),
        },
        store,
        upstream.client_tls(),
        PendingCodes::default(),
    ));
    let fake = Fake::new(Options::default());
    let key = crate::project::MaskedSecret {
        name: "ANTHROPIC_API_KEY".into(),
        real: REAL_API_KEY.into(),
        surrogate: "masked-api-key".into(),
    };
    let token = crate::project::MaskedSecret {
        name: "CLAUDE_CODE_OAUTH_TOKEN".into(),
        real: "sk-ant-oat01-REAL-ACCESS-0".into(),
        surrogate: "masked-oauth-token".into(),
    };
    let cfg = TestNetworkConfig {
        trust_cas: vec![upstream.ca_pem()],
        interceptors: vec![service],
        inject: vec![key, token],
        ..Default::default()
    };
    run_with_config(cfg, |proxy, _log, mitm| async move {
        let port = upstream.port();
        upstream.serve(fake.app());
        let req = hyper::Request::get("/v1/messages")
            .header("x-api-key", "masked-api-key")
            .body(http_body_util::Full::new(bytes::Bytes::new()))
            .unwrap();
        let resp = guest_request(&proxy, &mitm, port, false, req).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        let resp = guest_request(
            &proxy,
            &mitm,
            port,
            false,
            get_with_bearer("/v1/messages", "masked-oauth-token"),
        )
        .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
    });
    assert!(!db.has_database("services.grants"));
    assert!(!db.has_database("services.lookups"));
}

/// A service host is allowed without a rule, but a deny rule still wins.
#[test]
fn a_service_host_is_allowed_unless_denied() {
    let s = setup(Options::default());
    let port = s.upstream.port();
    let (_log, _ca, network) = build_network(s.config());
    let t = network.resolve_target("127.0.0.1", port);
    assert!(t.allowed && !t.is_passthrough() && t.interceptor.is_some());
    let t = network.resolve_target("127.0.0.1", port.wrapping_add(1));
    assert!(!t.allowed && t.interceptor.is_none());

    let mut network = network;
    network.deny_targets = vec![crate::network::target::NetworkTarget {
        host: "127.0.0.1".into(),
        port: Some(port),
    }];
    let t = network.resolve_target("127.0.0.1", port);
    assert!(!t.allowed && t.interceptor.is_none());

    // A passthrough rule does not take an owned host out of interception.
    network.deny_targets = vec![];
    network.passthrough_targets = vec![crate::network::target::NetworkTarget {
        host: "127.0.0.1".into(),
        port: None,
    }];
    let t = network.resolve_target("127.0.0.1", port);
    assert!(t.allowed && !t.is_passthrough());
}

/// Whatever authority the guest names (`Host`, an absolute-form target,
/// h2 `:authority`), the upstream sees the endpoint's.
#[test]
fn the_upstream_sees_the_endpoints_authority() {
    let s = setup_with(Options::default(), &[b"h2", b"http/1.1"], false);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let access = sign_in(&proxy, &mitm, &r).await["access_token"]
            .as_str()
            .unwrap()
            .to_string();
        let endpoint = format!("127.0.0.1:{}", r.api_port);
        let bearer = format!("Bearer {access}");
        for (h2, uri, host) in [
            (false, "/v1/messages", Some("attacker.example")),
            (
                false,
                "https://attacker.example/v1/messages",
                Some("attacker.example"),
            ),
            (true, "https://attacker.example/v1/messages", None),
            (
                true,
                "https://attacker.example:1/v1/messages",
                Some("other.example"),
            ),
        ] {
            let mut req = hyper::Request::get(uri).header("authorization", &bearer);
            if let Some(host) = host {
                req = req.header("host", host);
            }
            let req = req
                .body(http_body_util::Full::new(bytes::Bytes::new()))
                .unwrap();
            let resp = guest_request(&proxy, &mitm, r.api_port, h2, req).await;
            assert_eq!(resp.status, 200, "{uri}: {}", resp.body);
            let seen = r.fake.seen.last();
            assert_eq!(seen.authority.as_deref(), Some(endpoint.as_str()), "{uri}");
            assert_eq!(seen.path, "/v1/messages");
            let hosts: Vec<_> = seen.headers.get_all("host").iter().collect();
            assert!(hosts.len() <= 1, "{hosts:?}");
        }
    });
}

/// A new sign-in of the same account and scopes replaces the older grant
/// and revokes its real tokens upstream.
#[test]
fn a_new_sign_in_revokes_the_grant_it_replaces() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        sign_in(&proxy, &mitm, &r).await;
        let revoked = || {
            r.fake
                .seen
                .all()
                .into_iter()
                .filter(|s| s.path == "/v1/oauth/token/revoke")
                .map(|s| serde_json::from_str::<Value>(&s.body).unwrap()["token"].clone())
                .collect::<Vec<_>>()
        };
        assert!(revoked().is_empty());
        sign_in(&proxy, &mitm, &r).await;
        for _ in 0..50 {
            if !revoked().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(revoked(), ["sk-ant-ort01-REAL-REFRESH-0"]);
        assert_eq!(list_grants(&r.home.db).await.unwrap().len(), 1);
    });
}

/// At most three API keys per sign-in and hour; then a local 429.
#[test]
fn create_api_key_is_limited_per_grant() {
    let s = setup(Options::default());
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        for n in 0..4 {
            let mut req = post(
                "/api/oauth/claude_cli/create_api_key",
                "application/json",
                "",
            );
            req.headers_mut()
                .insert("authorization", format!("Bearer {access}").parse().unwrap());
            let before = r.fake.seen.all().len();
            let resp = guest_request(&proxy, &mitm, r.port, false, req).await;
            if n < 3 {
                assert_eq!(resp.status, 200, "{}", resp.body);
            } else {
                assert_eq!(resp.status, 429, "{}", resp.body);
                assert_eq!(resp.json()["error"]["type"], "rate_limit_error");
                assert_eq!(r.fake.seen.all().len(), before, "answered locally");
            }
        }
    });
}

/// An enabled service that cannot run (no token store) denies its hosts,
/// even where a rule allows everything.
#[test]
fn the_hosts_of_an_unavailable_service_are_denied() {
    let cfg = TestNetworkConfig {
        allowed_hosts: vec!["*".into()],
        unavailable_targets: crate::services::ServiceId::Anthropic.targets(),
        ..Default::default()
    };
    let (_log, _ca, network) = build_network(cfg);
    for host in ["api.anthropic.com", "PLATFORM.claude.com."] {
        let t = network.resolve_target(host, 443);
        assert!(!t.allowed && t.interceptor.is_none(), "{host}");
    }
    assert!(network.resolve_target("api.anthropic.com", 80).allowed);
    assert!(network.resolve_target("example.com", 443).allowed);
}
