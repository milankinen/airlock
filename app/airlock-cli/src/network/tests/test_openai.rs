//! The `openai` service end to end: Codex's ChatGPT sign-in and its
//! chatgpt.com traffic (HTTP and the WebSocket upgrade) through the proxy,
//! against a fake provider.

use std::rc::Rc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use axum::Router;
use axum::response::IntoResponse;
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

use super::fake_provider::*;
use super::helpers::*;
use crate::network::interceptor::Interceptor;
use crate::services::ServiceId;
use crate::services::auth_codes::{Channel, PendingCodes};
use crate::services::openai::{Endpoints, Openai};
use crate::services::store::{TokenStore, list_grants, now_ms};
use crate::services::tokens::{FAKE_JWT_PREFIX, TokenKind};

/// The code the fake provider issued: what the browser brings back.
const REAL_CODE: &str = "real-code";
/// The code of a device-code sign-in.
const REAL_DEVICE_CODE: &str = "real-device-code";

/// The `n`th real refresh token the fake issues.
fn real_refresh(n: usize) -> String {
    format!("v1.REAL-RT-{n}-{}", "x_Y-".repeat(15))
}

/// The guest's code exchange (a form) with `code`.
fn code_exchange(code: &str) -> String {
    code_exchange_to(code, "http://127.0.0.1:1455/auth/callback")
}

/// The guest's code exchange with `code` and `redirect_uri`.
fn code_exchange_to(code: &str, redirect_uri: &str) -> String {
    url::form_urlencoded::Serializer::new(String::new())
        .append_pair("grant_type", "authorization_code")
        .append_pair("code", code)
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("client_id", "app_EMoamEEZ73f0CkXaXp7hrann")
        .append_pair("code_verifier", "v")
        .finish()
}

fn form_fields(body: &str) -> std::collections::BTreeMap<String, String> {
    url::form_urlencoded::parse(body.as_bytes())
        .into_owned()
        .collect()
}

/// A JWT with `claims` and a signature that marks it as real.
fn jwt(claims: &Value, n: usize) -> String {
    format!(
        "{}.{}.REALSIG{n}",
        URL_SAFE_NO_PAD.encode(br#"{"alg":"RS256","typ":"JWT"}"#),
        URL_SAFE_NO_PAD.encode(claims.to_string()),
    )
}

fn claims_of(token: &str) -> Value {
    let payload = token.split('.').nth(1).unwrap();
    serde_json::from_slice(&URL_SAFE_NO_PAD.decode(payload).unwrap()).unwrap()
}

struct Fake {
    seen: SeenLog,
    access: Mutex<String>,
    refresh: Mutex<String>,
    /// Lifetime of the access tokens the fake issues, in seconds.
    access_lifetime: i64,
    refreshes: AtomicUsize,
    /// The plan in the ID token a refresh issues.
    refreshed_plan: Mutex<Option<String>>,
    /// Fields a refresh answer carries besides the tokens.
    refresh_extra: Mutex<Option<Value>>,
    /// Fields the code exchange answer carries besides (or instead of)
    /// the usual ones.
    exchange_extra: Mutex<Option<Value>>,
}

impl Fake {
    fn new(access_lifetime: i64) -> Arc<Self> {
        let fake = Self {
            seen: SeenLog::default(),
            access: Mutex::new(String::new()),
            refresh: Mutex::new(real_refresh(0)),
            access_lifetime,
            refreshes: AtomicUsize::new(0),
            refreshed_plan: Mutex::new(None),
            refresh_extra: Mutex::new(None),
            exchange_extra: Mutex::new(None),
        };
        *fake.access.lock().unwrap() = fake.issue_access(0);
        Arc::new(fake)
    }

    fn issue_access(&self, n: usize) -> String {
        jwt(
            &json!({
                "exp": now_ms() / 1000 + self.access_lifetime,
                "https://api.openai.com/auth": { "chatgpt_plan_type": "plus" },
            }),
            n,
        )
    }

    fn access(&self) -> String {
        self.access.lock().unwrap().clone()
    }

    fn id_token() -> String {
        Self::id_token_with_plan("plus")
    }

    fn id_token_with_plan(plan: &str) -> String {
        jwt(
            &json!({
                "email": "o@example.com",
                "sub": "user-1",
                // Fixed: the tests compare the ID token of two calls.
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

    async fn handle(&self, req: axum::extract::Request) -> axum::response::Response {
        let seen = self.seen.record(req).await;
        match (seen.method.as_str(), seen.path.as_str()) {
            ("POST", "/oauth/token") => return self.token(&seen),
            ("POST", "/oauth/revoke") => return axum::Json(json!({})).into_response(),
            ("POST", "/api/accounts/deviceauth/token") => {
                return axum::Json(json!({
                    "authorization_code": REAL_DEVICE_CODE,
                    "code_challenge": "c",
                    "code_verifier": "v",
                }))
                .into_response();
            }
            _ => {}
        }
        let expected = format!("Bearer {}", self.access());
        if seen.header("authorization") == Some(expected.as_str()) {
            match seen.path.as_str() {
                "/backend-api/leak/access" => {
                    axum::Json(json!({ "t": [self.access()] })).into_response()
                }
                "/backend-api/leak/refresh" => {
                    let refresh = self.refresh.lock().unwrap().clone();
                    axum::Json(json!({ "t": { "u": refresh } })).into_response()
                }
                "/backend-api/json" => {
                    axum::Json(json!({ "id": "rt_short", "jwt": "a.b.c" })).into_response()
                }
                _ => "ok".into_response(),
            }
        } else {
            (axum::http::StatusCode::UNAUTHORIZED, "no").into_response()
        }
    }

    fn token(&self, seen: &Seen) -> axum::response::Response {
        let form = form_fields(&seen.body);
        if form.get("grant_type").map(String::as_str) == Some("authorization_code") {
            if ![REAL_CODE, REAL_DEVICE_CODE].contains(&form["code"].as_str()) {
                return (
                    axum::http::StatusCode::BAD_REQUEST,
                    axum::Json(json!({ "error": "invalid_grant" })),
                )
                    .into_response();
            }
            let mut answer = json!({
                "id_token": Self::id_token(),
                "access_token": self.access(),
                "refresh_token": *self.refresh.lock().unwrap(),
                "expires_in": self.access_lifetime,
            });
            if let Some(Value::Object(extra)) = &*self.exchange_extra.lock().unwrap() {
                answer.as_object_mut().unwrap().extend(extra.clone());
            }
            return axum::Json(answer).into_response();
        }
        let body: Value = serde_json::from_str(&seen.body).unwrap_or_default();
        let current = self.refresh.lock().unwrap().clone();
        if body["grant_type"] != "refresh_token" || body["refresh_token"] != current.as_str() {
            return (
                axum::http::StatusCode::UNAUTHORIZED,
                axum::Json(json!({ "error": { "code": "refresh_token_invalidated" } })),
            )
                .into_response();
        }
        let n = self.refreshes.fetch_add(1, Ordering::SeqCst) + 1;
        *self.access.lock().unwrap() = self.issue_access(n);
        *self.refresh.lock().unwrap() = real_refresh(n);
        let mut answer = json!({
            "access_token": self.access(),
            "refresh_token": *self.refresh.lock().unwrap(),
        });
        if let Some(plan) = &*self.refreshed_plan.lock().unwrap() {
            answer["id_token"] = Self::id_token_with_plan(plan).into();
        }
        if let Some(Value::Object(extra)) = &*self.refresh_extra.lock().unwrap() {
            answer.as_object_mut().unwrap().extend(extra.clone());
        }
        axum::Json(answer).into_response()
    }
}

struct Setup {
    auth: FakeUpstream,
    chatgpt: FakeUpstream,
    /// The chatgpt host is a raw WebSocket endpoint (for the upgrade
    /// test), not the fake provider.
    websocket: bool,
    fake: Arc<Fake>,
    home: StoreHome,
    store: Arc<TokenStore>,
    codes: PendingCodes,
    service: Rc<dyn Interceptor>,
}

/// Both hosts serve the fake provider.
fn setup(access_lifetime: i64) -> Setup {
    setup_with(access_lifetime, false)
}

fn setup_with(access_lifetime: i64, websocket: bool) -> Setup {
    let auth = FakeUpstream::bind(&[b"h2", b"http/1.1"]);
    let chatgpt = FakeUpstream::bind(&[b"h2", b"http/1.1"]);
    let (home, store) = test_store();
    let endpoints = Endpoints {
        auth: auth.endpoint(),
        chatgpt: chatgpt.endpoint(),
    };
    let codes = PendingCodes::default();
    let service: Rc<dyn Interceptor> = Rc::new(Openai::new(
        endpoints,
        store.clone(),
        auth.client_tls(),
        codes.clone(),
    ));
    Setup {
        auth,
        chatgpt,
        websocket,
        fake: Fake::new(access_lifetime),
        home,
        store,
        codes,
        service,
    }
}

struct Running {
    auth_port: u16,
    chatgpt_port: u16,
    fake: Arc<Fake>,
    store: Arc<TokenStore>,
    codes: PendingCodes,
    home: StoreHome,
    /// Every byte the WebSocket endpoint read.
    ws_read: Arc<Mutex<Vec<u8>>>,
}

impl Setup {
    fn config(&self) -> TestNetworkConfig {
        TestNetworkConfig {
            allowed_hosts: vec![],
            trust_cas: vec![self.auth.ca_pem(), self.chatgpt.ca_pem()],
            interceptors: vec![self.service.clone()],
            ..Default::default()
        }
    }

    fn serve(self) -> Running {
        let (auth_port, chatgpt_port) = (self.auth.port(), self.chatgpt.port());
        self.auth.serve(self.fake.app());
        let ws_read = Arc::new(Mutex::new(Vec::new()));
        if self.websocket {
            self.chatgpt.serve_upgrade_echo(ws_read.clone());
        } else {
            self.chatgpt.serve(self.fake.app());
        }
        Running {
            auth_port,
            chatgpt_port,
            fake: self.fake,
            store: self.store,
            codes: self.codes,
            home: self.home,
            ws_read,
        }
    }
}

type Proxy = airlock_common::network_capnp::network_proxy::Client;

/// The guest's code exchange, with the surrogate code the callback
/// forward handed it; returns the answer.
async fn sign_in(proxy: &Proxy, mitm: &str, r: &Running) -> Value {
    let code = r
        .codes
        .issue(REAL_CODE, ServiceId::Openai, Channel::Callback(1455))
        .unwrap();
    exchange(proxy, mitm, r, &code).await
}

async fn exchange(proxy: &Proxy, mitm: &str, r: &Running, code: &str) -> Value {
    let resp = guest_request(
        proxy,
        mitm,
        r.auth_port,
        false,
        post(
            "/oauth/token",
            "application/x-www-form-urlencoded",
            &code_exchange(code),
        ),
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

#[test]
fn a_code_exchange_gives_the_guest_fake_jwts_and_stores_the_real_tokens() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let (id_token, access, refresh) = (
            answer["id_token"].as_str().unwrap(),
            answer["access_token"].as_str().unwrap(),
            answer["refresh_token"].as_str().unwrap(),
        );
        assert!(refresh.starts_with("airlock-rt-"), "{refresh}");
        // The fake JWT's `exp` is the real token's own, not a synthetic one.
        for (token, real_exp) in [
            (
                id_token,
                claims_of(&Fake::id_token())["exp"].as_i64().unwrap(),
            ),
            (access, claims_of(&r.fake.access())["exp"].as_i64().unwrap()),
        ] {
            let parts: Vec<&str> = token.split('.').collect();
            assert_eq!(parts.len(), 3, "{token}");
            assert!(!token.contains('='), "unpadded: {token}");
            let header: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
            assert_eq!(header, json!({ "alg": "none", "typ": "JWT" }));
            let claims = claims_of(token);
            assert_eq!(claims["exp"].as_i64().unwrap(), real_exp);
            assert!(claims["airlock_nonce"].is_string());
        }
        let id_claims = claims_of(id_token);
        assert_eq!(id_claims["email"], "o@example.com");
        assert_eq!(
            id_claims["https://api.openai.com/auth"]["chatgpt_account_id"],
            "acc-1"
        );
        assert_eq!(
            claims_of(access)["https://api.openai.com/auth"]["chatgpt_plan_type"],
            "plus"
        );

        let mut want = form_fields(&code_exchange(REAL_CODE));
        want.insert("code".into(), REAL_CODE.into());
        assert_eq!(
            form_fields(&r.fake.seen.last().body),
            want,
            "forwarded with the real code"
        );
        let grant = r
            .store
            .grant_of(ServiceId::Openai, access, &[TokenKind::Access])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            grant.real(TokenKind::Access),
            Some(r.fake.access().as_str())
        );
        assert_eq!(
            grant.real(TokenKind::Refresh),
            Some(real_refresh(0).as_str())
        );
        assert_eq!(grant.real(TokenKind::Id), Some(Fake::id_token().as_str()));
        assert_eq!(grant.client_id, "app_EMoamEEZ73f0CkXaXp7hrann");
        let listed = list_grants(&r.home.db).await.unwrap();
        assert_eq!(listed[0].account.as_deref(), Some("o@example.com"));
        assert_eq!(listed[0].service, "openai");
    });
}

#[test]
fn chatgpt_requests_carry_the_real_token_over_h1_and_h2() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        for h2 in [false, true] {
            let resp = guest_request(
                &proxy,
                &mitm,
                r.chatgpt_port,
                h2,
                get_with_bearer("/backend-api/codex/models", access),
            )
            .await;
            assert_eq!(resp.status, 200, "h2={h2}: {}", resp.body);
            assert_eq!(
                r.fake.seen.last().header("authorization"),
                Some(format!("Bearer {}", r.fake.access()).as_str())
            );
        }
    });
}

#[test]
fn the_websocket_upgrade_carries_the_real_token() {
    let s = setup_with(3600, true);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();

        let conn = TestConnection::connect(&proxy, "127.0.0.1", r.chatgpt_port)
            .await
            .unwrap();
        let tls = tokio_rustls::TlsConnector::from(Arc::new(tls_trusting(&mitm)));
        let mut stream = tls
            .connect(
                rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap(),
                conn.into_stream(),
            )
            .await
            .unwrap();
        let handshake = websocket_handshake(r.chatgpt_port, true)
            .replacen("GET /ws ", "GET /backend-api/codex/responses ", 1)
            .replacen(
                "\r\n",
                &format!("\r\nAuthorization: Bearer {access}\r\n"),
                1,
            );
        assert_raw_relay(&mut stream, &handshake, "HTTP/1.1 101").await;

        let read = String::from_utf8_lossy(&r.ws_read.lock().unwrap()).to_lowercase();
        let real = format!("authorization: bearer {}", r.fake.access()).to_lowercase();
        assert!(read.contains(&real), "upstream read: {read}");
        assert!(
            !read.contains(&access.to_lowercase()),
            "the surrogate went upstream"
        );
    });
}

/// A token-exchange grant type and an unknown refresh surrogate are both
/// refused without reaching the provider.
#[test]
fn token_exchange_and_an_unknown_refresh_are_refused_locally() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let before = r.fake.seen.all().len();

        let exchange = format!(
            "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Atoken-exchange&client_id=app_EMoamEEZ73f0CkXaXp7hrann&requested_token=openai-api-key&subject_token={}&subject_token_type=urn%3Aietf%3Aparams%3Aoauth%3Atoken-type%3Aid_token",
            answer["id_token"].as_str().unwrap()
        );
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post(
                "/oauth/token",
                "application/x-www-form-urlencoded",
                &exchange,
            ),
        )
        .await;
        assert_eq!(resp.status, 400);
        assert_eq!(resp.json()["error"], "unsupported_grant_type");

        let unknown = json!({ "grant_type": "refresh_token", "refresh_token": "airlock-rt-nope" });
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post("/oauth/token", "application/json", &unknown.to_string()),
        )
        .await;
        assert_eq!(resp.status, 401);
        assert_eq!(resp.json()["error"], "refresh_token_invalidated");

        assert_eq!(
            r.fake.seen.all().len(),
            before,
            "nothing reached the provider"
        );
    });
}

/// `codex login` revokes before it signs in: the grant is deleted and the
/// real token revoked upstream.
#[test]
fn a_revoke_deletes_the_grant_and_revokes_the_real_token_upstream() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let revoke = json!({
            "token": answer["refresh_token"],
            "token_type_hint": "refresh_token",
            "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
        });
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post("/oauth/revoke", "application/json", &revoke.to_string()),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.json(), json!({}));
        // Both real tokens: the refresh token first, then the access token.
        let revokes: Vec<Value> = r
            .fake
            .seen
            .all()
            .into_iter()
            .filter(|s| s.path == "/oauth/revoke")
            .map(|s| serde_json::from_str(&s.body).unwrap())
            .collect();
        assert_eq!(
            revokes,
            [
                json!({
                    "token": real_refresh(0),
                    "token_type_hint": "refresh_token",
                    "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
                }),
                // Codex's fallback revoke of the access token names no
                // client.
                json!({
                    "token": r.fake.access(),
                    "token_type_hint": "access_token",
                }),
            ]
        );
        assert!(list_grants(&r.home.db).await.unwrap().is_empty());
        // An unknown token is answered locally.
        let before = r.fake.seen.all().len();
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post("/oauth/revoke", "application/json", &revoke.to_string()),
        )
        .await;
        assert_eq!(resp.status, 200);
        assert_eq!(r.fake.seen.all().len(), before);
    });
}

/// The device-code sign-in: the code of the poll answer reaches the guest
/// as a surrogate code, which its exchange swaps back.
#[test]
fn the_device_code_reaches_the_guest_as_a_surrogate() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let poll = json!({ "device_auth_id": "d", "user_code": "U" });
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post(
                "/api/accounts/deviceauth/token",
                "application/json",
                &poll.to_string(),
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        let code = resp.json()["authorization_code"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(code.starts_with("airlock-code-"), "{code}");
        assert_eq!(resp.json()["code_verifier"], "v");
        let device_exchange = |redirect: &str| {
            post(
                "/oauth/token",
                "application/x-www-form-urlencoded",
                &code_exchange_to(&code, redirect),
            )
        };
        // Bound to the device flow: a loopback exchange cannot spend it
        // (and uses it up), so poll again.
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            device_exchange("http://127.0.0.1:1455/auth/callback"),
        )
        .await;
        assert_eq!(resp.status, 400, "{}", resp.body);
        assert_eq!(resp.json()["error"], "invalid_grant");
        assert_eq!(r.fake.seen.last().path, "/api/accounts/deviceauth/token");

        let code = r
            .codes
            .issue(REAL_DEVICE_CODE, ServiceId::Openai, Channel::Device)
            .unwrap();
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post(
                "/oauth/token",
                "application/x-www-form-urlencoded",
                &code_exchange_to(&code, "https://auth.openai.com/deviceauth/callback"),
            ),
        )
        .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(
            form_fields(&r.fake.seen.last().body)["code"],
            REAL_DEVICE_CODE
        );
    });
}

/// A code of one service cannot be redeemed by another: the anthropic
/// callback's surrogate code is refused by the openai exchange.
#[test]
fn a_code_of_another_service_is_refused() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let code = r
            .codes
            .issue(REAL_CODE, ServiceId::Anthropic, Channel::Callback(1455))
            .unwrap();
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post(
                "/oauth/token",
                "application/x-www-form-urlencoded",
                &code_exchange(&code),
            ),
        )
        .await;
        assert_eq!(resp.status, 400, "{}", resp.body);
        assert_eq!(resp.json()["error"], "invalid_grant");
        assert!(r.fake.seen.all().is_empty());
    });
}

/// The agent's own refresh is relayed upstream: the refresh surrogate
/// stays the same, and a new ID token gets a surrogate with its new
/// claims.
#[test]
fn a_relayed_refresh_carries_the_claims_of_the_upstream_refresh() {
    let s = setup(60);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        *r.fake.refreshed_plan.lock().unwrap() = Some("pro".into());
        let refresh = json!({
            "client_id": "another-client",
            "grant_type": "refresh_token",
            "refresh_token": answer["refresh_token"],
            "scope": "openid api.everything",
            "extra": "x",
        });
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post("/oauth/token", "application/json", &refresh.to_string()),
        )
        .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(r.fake.refreshes.load(Ordering::SeqCst), 1);
        // Only the provider's own fields went upstream.
        let sent: Value = serde_json::from_str(&r.fake.seen.last().body).unwrap();
        assert_eq!(
            sent,
            json!({
                "client_id": "app_EMoamEEZ73f0CkXaXp7hrann",
                "grant_type": "refresh_token",
                "refresh_token": real_refresh(0),
            })
        );
        let refreshed = resp.json();
        assert_eq!(refreshed["refresh_token"], answer["refresh_token"]);
        let claims = claims_of(refreshed["id_token"].as_str().unwrap());
        assert_eq!(
            claims["https://api.openai.com/auth"]["chatgpt_plan_type"],
            "pro"
        );
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
    });
}

/// A real token anywhere in a refresh answer reaches the guest as a
/// surrogate; a string in no known format under a token-like key refuses
/// the answer, and its tokens are not stored.
#[test]
fn a_refresh_answer_is_swapped_or_refused_whole() {
    let s = setup(60);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let refresh = json!({
            "grant_type": "refresh_token",
            "refresh_token": answer["refresh_token"],
        });
        let send = || {
            guest_request(
                &proxy,
                &mitm,
                r.auth_port,
                false,
                post("/oauth/token", "application/json", &refresh.to_string()),
            )
        };
        *r.fake.refresh_extra.lock().unwrap() = Some(json!({ "session": Fake::id_token() }));
        let resp = send().await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
        let session = resp.json()["session"].as_str().unwrap().to_string();
        assert!(session.starts_with(FAKE_JWT_PREFIX), "{session}");

        *r.fake.refresh_extra.lock().unwrap() =
            Some(json!({ "session_token": "opaque-secret-value-0123456789" }));
        let resp = send().await;
        assert_eq!(resp.status, 502, "{}", resp.body);
        let grant = r
            .store
            .grant_of(
                ServiceId::Openai,
                answer["refresh_token"].as_str().unwrap(),
                &[TokenKind::Refresh],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            grant.real(TokenKind::Refresh),
            Some(real_refresh(1).as_str()),
            "the refused refresh stored nothing"
        );
    });
}

/// The refresh token's format is not known: whatever the provider issues
/// under `refresh_token` (a short opaque value, `v1.…`) gets an
/// `airlock-rt-…` surrogate, and the real value goes back upstream on
/// refresh and revoke.
#[test]
fn refresh_tokens_of_any_format_get_surrogates() {
    let s = setup(60);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        *r.fake.refresh.lock().unwrap() = "REAL-short".into();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let surrogate = answer["refresh_token"].as_str().unwrap().to_string();
        assert!(surrogate.starts_with("airlock-rt-"), "{surrogate}");
        let refresh = json!({ "grant_type": "refresh_token", "refresh_token": surrogate });
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post("/oauth/token", "application/json", &refresh.to_string()),
        )
        .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
        let sent: Value = serde_json::from_str(&r.fake.seen.last().body).unwrap();
        assert_eq!(sent["refresh_token"], "REAL-short");
        assert_eq!(
            resp.json()["refresh_token"],
            surrogate.as_str(),
            "stays the same"
        );
        assert!(real_refresh(1).starts_with("v1."));
        let revoke = json!({ "token": surrogate });
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post("/oauth/revoke", "application/json", &revoke.to_string()),
        )
        .await;
        assert_eq!(resp.status, 200);
        let revoked: Vec<Value> = r
            .fake
            .seen
            .all()
            .into_iter()
            .filter(|s| s.path == "/oauth/revoke")
            .map(|s| serde_json::from_str(&s.body).unwrap())
            .collect();
        assert_eq!(revoked[0]["token"], real_refresh(1));
    });
}

/// An access token that is no JWT gets an opaque surrogate, which works
/// on chatgpt.com.
#[test]
fn an_opaque_access_token_gets_an_opaque_surrogate() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        *r.fake.access.lock().unwrap() = "REAL-opaque-access-token".into();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        assert!(access.starts_with("airlock-at-"), "{access}");
        let resp = guest_request(
            &proxy,
            &mitm,
            r.chatgpt_port,
            false,
            get_with_bearer("/backend-api/codex/models", access),
        )
        .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(
            r.fake.seen.last().header("authorization"),
            Some("Bearer REAL-opaque-access-token")
        );
    });
}

/// chatgpt.com API answers ask for no compression, and one with a real
/// access, ID or refresh token is refused; other JSON passes.
#[test]
fn chatgpt_api_answers_with_a_real_token_are_refused() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        for (path, refused) in [
            ("/backend-api/leak/access", true),
            ("/backend-api/leak/refresh", true),
            ("/backend-api/json", false),
        ] {
            let mut req = get_with_bearer(path, access);
            req.headers_mut()
                .insert("accept-encoding", "gzip".parse().unwrap());
            let resp = guest_request(&proxy, &mitm, r.chatgpt_port, false, req).await;
            assert_eq!(resp.status == 502, refused, "{path}: {}", resp.body);
            assert!(!resp.body.contains("REAL"), "{path}: {}", resp.body);
            assert_eq!(
                r.fake.seen.last().header("accept-encoding"),
                Some("identity")
            );
        }
    });
}

/// The real token goes to every chatgpt.com path, whatever its spelling,
/// and never to the auth host, whose other routes are forbidden.
#[test]
fn the_real_token_goes_to_chatgpt_only() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let answer = sign_in(&proxy, &mitm, &r).await;
        let access = answer["access_token"].as_str().unwrap();
        let real = format!("Bearer {}", r.fake.access());
        for path in [
            "/backend-api/codex/models",
            "/backend-apix/codex",
            "/",
            "//Backend-API/codex/models",
            "/backend-api/%2e%2e/x",
            "/backend-api/codex/",
        ] {
            let resp = guest_request(
                &proxy,
                &mitm,
                r.chatgpt_port,
                false,
                get_with_bearer(path, access),
            )
            .await;
            assert_eq!(resp.status, 200, "{path}: {}", resp.body);
            assert_eq!(
                r.fake.seen.last().header("authorization"),
                Some(real.as_str()),
                "{path}"
            );
        }
        let before = r.fake.seen.all().len();
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            get_with_bearer("/backend-api/codex/models", access),
        )
        .await;
        assert_eq!(resp.status, 403, "{}", resp.body);
        assert_eq!(r.fake.seen.all().len(), before, "not forwarded");
    });
}

/// An exchange with a code that is no surrogate code is refused locally.
#[test]
fn an_exchange_needs_a_surrogate_code() {
    let s = setup(3600);
    run_with_config(s.config(), |proxy, _log, mitm| async move {
        let r = s.serve();
        let resp = guest_request(
            &proxy,
            &mitm,
            r.auth_port,
            false,
            post(
                "/oauth/token",
                "application/x-www-form-urlencoded",
                &code_exchange(REAL_CODE),
            ),
        )
        .await;
        assert_eq!(resp.status, 400);
        assert_eq!(resp.json()["error"], "invalid_grant");
        assert!(r.fake.seen.all().is_empty());
    });
}
