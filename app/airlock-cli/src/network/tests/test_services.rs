//! How the services dispatch a request, called directly (no proxy, no
//! upstream): which host spellings they own, the authority they pin, and
//! the backstops on what they pass on. `next` stands for the upstream and
//! records what it got.

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt as _, Either, Full};
use hyper::{Request, Response, StatusCode, Version};
use serde_json::{Value, json};

use super::fake_provider::{StoreHome, test_store, tls_trusting};
use super::helpers::block_on_local;
use crate::network::http::ResponseBody;
use crate::network::interceptor::{Interceptor, Next};
use crate::network::target::Endpoint;
use crate::services::auth_codes::PendingCodes;
use crate::services::store::{NewGrant, TokenStore};
use crate::services::tokens::{Token, TokenKind};
use crate::services::{ServiceId, anthropic, openai};

/// What the upstream got: URI, `Host`, the other headers, the body.
#[derive(Clone, Debug)]
struct Got {
    uri: String,
    host: Option<String>,
    headers: hyper::HeaderMap,
    body: String,
}

type Log = Rc<RefCell<Vec<Got>>>;

/// An upstream that records each request and answers `status`, `content
/// type` and `body` (with its length).
fn upstream(log: &Log, content_type: &'static str, body: &str) -> Next {
    let (log, body) = (log.clone(), body.to_string());
    Box::new(move |req: Request<ResponseBody>| {
        Box::pin(async move {
            let (parts, req_body) = req.into_parts();
            let req_body = req_body.collect().await.unwrap().to_bytes();
            log.borrow_mut().push(Got {
                uri: parts.uri.to_string(),
                host: parts
                    .headers
                    .get("host")
                    .map(|h| h.to_str().unwrap().to_string()),
                headers: parts.headers,
                body: String::from_utf8_lossy(&req_body).into_owned(),
            });
            let mut resp = Response::new(Either::Right(Full::new(Bytes::from(body.clone()))));
            *resp.status_mut() = StatusCode::OK;
            resp.headers_mut()
                .insert("content-type", content_type.parse().unwrap());
            resp.headers_mut()
                .insert("content-length", body.len().to_string().parse().unwrap());
            Ok(resp)
        })
    })
}

fn request(method: &str, uri: &str, headers: &[(&str, &str)], body: &str) -> Request<ResponseBody> {
    let mut req = Request::builder().method(method).uri(uri);
    for (k, v) in headers {
        req = req.header(*k, *v);
    }
    req.body(Either::Right(Full::new(Bytes::from(body.to_string()))))
        .unwrap()
}

async fn body_of(resp: Response<ResponseBody>) -> (StatusCode, String) {
    let status = resp.status();
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    (status, String::from_utf8_lossy(&body).into_owned())
}

/// The production services' interceptors, with a store in a temp home.
fn services() -> (StoreHome, Rc<dyn Interceptor>, Rc<dyn Interceptor>) {
    let (dir, _store, anthropic, openai) = services_with_store();
    (dir, anthropic, openai)
}

/// [`services`] and their store.
fn services_with_store() -> (
    StoreHome,
    Arc<TokenStore>,
    Rc<dyn Interceptor>,
    Rc<dyn Interceptor>,
) {
    let (dir, store) = test_store();
    let tls = Arc::new(tls_trusting(""));
    let codes = PendingCodes::default();
    let anthropic: Rc<dyn Interceptor> = Rc::new(anthropic::Anthropic::new(
        anthropic::Endpoints::production(),
        store.clone(),
        tls.clone(),
        codes.clone(),
    ));
    let openai: Rc<dyn Interceptor> = Rc::new(openai::Openai::new(
        openai::Endpoints::production(),
        store.clone(),
        tls,
        codes,
    ));
    (dir, store, anthropic, openai)
}

/// The guest's DNS keeps the case of a name, and a trailing dot names the
/// same host: such spellings get the owned host's handling, never a raw
/// forward. Here the device poll's real code becomes a surrogate.
#[test]
fn host_spellings_get_the_owned_hosts_handling() {
    block_on_local(async {
        let (_dir, _anthropic, openai) = services();
        for host in ["AUTH.OPENAI.COM", "Auth.OpenAI.com.", "auth.openai.com."] {
            let log = Log::default();
            let poll = request("POST", "/api/accounts/deviceauth/token", &[], "{}");
            let next = upstream(
                &log,
                "application/json",
                r#"{"authorization_code":"real-device-code","code_verifier":"v"}"#,
            );
            let resp = openai
                .send(&Endpoint::new(host, 443), poll, &[], next)
                .await
                .unwrap();
            let (status, body) = body_of(resp).await;
            assert_eq!(status, 200, "{host}: {body}");
            assert!(!body.contains("real-device-code"), "{host}: {body}");
            assert!(body.contains("airlock-code-"), "{host}: {body}");
        }
    });
}

#[test]
fn host_spellings_get_the_token_endpoint_and_the_strict_api() {
    block_on_local(async {
        let (_dir, anthropic, _openai) = services();
        // An exchange with a real code: refused locally.
        let log = Log::default();
        let exchange = json!({
            "grant_type": "authorization_code",
            "code": "real-code",
            "redirect_uri": "http://localhost:40000/callback",
        });
        let req = request(
            "POST",
            "/v1/oauth/token",
            &[("content-type", "application/json")],
            &exchange.to_string(),
        );
        let resp = anthropic
            .send(
                &Endpoint::new("Platform.Claude.com", 443),
                req,
                &[],
                upstream(&log, "application/json", "{}"),
            )
            .await
            .unwrap();
        let (status, body) = body_of(resp).await;
        assert_eq!(status, 400, "{body}");
        assert!(body.contains("invalid_grant"), "{body}");
        assert!(log.borrow().is_empty());

        // A real token on the API host: refused locally.
        let req = request(
            "GET",
            "/v1/messages",
            &[("authorization", "Bearer sk-ant-oat01-REAL")],
            "",
        );
        let resp = anthropic
            .send(
                &Endpoint::new("api.anthropic.com.", 443),
                req,
                &[],
                upstream(&log, "application/json", "{}"),
            )
            .await
            .unwrap();
        let (status, body) = body_of(resp).await;
        assert_eq!(status, 401, "{body}");
        assert!(log.borrow().is_empty());
    });
}

/// A host the service does not know (it should not get one) passes the
/// backstop: never a raw forward.
#[test]
fn an_unknown_endpoint_fails_closed() {
    block_on_local(async {
        let (_dir, anthropic, openai) = services();
        for service in [&anthropic, &openai] {
            let log = Log::default();
            let resp = service
                .send(
                    &Endpoint::new("other.example", 443),
                    request("GET", "/x", &[], ""),
                    &[],
                    upstream(&log, "application/json", r#"{"access_token":"real"}"#),
                )
                .await
                .unwrap();
            let (status, body) = body_of(resp).await;
            assert_eq!(status, 502, "{body}");
            assert!(!body.contains("real"), "{body}");
        }
    });
}

/// The upstream sees the endpoint's authority, whatever the guest named.
#[test]
fn the_authority_is_pinned_to_the_endpoint() {
    block_on_local(async {
        let (_dir, anthropic, _openai) = services();
        let api = Endpoint::new("api.anthropic.com", 443);
        let log = Log::default();
        for uri in [
            "/v1/messages?x=1",
            "https://attacker.example/v1/messages?x=1",
        ] {
            let req = request("GET", uri, &[("host", "attacker.example")], "");
            anthropic
                .send(&api, req, &[], upstream(&log, "text/plain", "ok"))
                .await
                .unwrap();
        }
        let mut h2 = request(
            "GET",
            "https://attacker.example:8443/v1/messages?x=1",
            &[],
            "",
        );
        *h2.version_mut() = Version::HTTP_2;
        anthropic
            .send(&api, h2, &[], upstream(&log, "text/plain", "ok"))
            .await
            .unwrap();
        let got: Vec<(String, Option<String>)> = log
            .borrow()
            .iter()
            .map(|g| (g.uri.clone(), g.host.clone()))
            .collect();
        let h1 = (
            "/v1/messages?x=1".to_string(),
            Some("api.anthropic.com".to_string()),
        );
        assert_eq!(
            got,
            [
                h1.clone(),
                h1,
                (
                    "https://api.anthropic.com/v1/messages?x=1".to_string(),
                    None
                ),
            ]
        );
    });
}

/// The routes of the token hosts that are no token endpoint pass the
/// backstop: tokens and codes in a JSON answer are refused, a surrogate
/// code passes.
#[test]
fn allowed_token_host_routes_pass_the_backstop() {
    block_on_local(async {
        let (_dir, anthropic, openai) = services();
        let auth = Endpoint::new("auth.openai.com", 443);
        let platform = Endpoint::new("platform.claude.com", 443);
        for (answer, refused) in [
            (json!({ "accessToken": "x", "access_token": "real" }), true),
            (json!({ "code": "real-code" }), true),
            (json!({ "authorization_code": "real-code" }), true),
            (json!({ "data": { "code_verifier": "v" } }), true),
            (json!({ "code": "airlock-code-x" }), false),
            (json!({ "user_code": "U", "device_auth_id": "d" }), false),
        ] {
            for (service, to, method, path) in [
                (&openai, &auth, "POST", "/api/accounts/deviceauth/usercode"),
                (&anthropic, &platform, "GET", "/v1/oauth/hello"),
            ] {
                let log = Log::default();
                let resp = service
                    .send(
                        to,
                        request(method, path, &[], "{}"),
                        &[],
                        upstream(&log, "application/json", &answer.to_string()),
                    )
                    .await
                    .unwrap();
                let (status, _) = body_of(resp).await;
                assert_eq!(status == 502, refused, "{path}: {answer}");
                assert_eq!(log.borrow().len(), 1, "{path}: forwarded");
            }
        }
    });
}

/// An allowed route goes upstream with its own path, never the guest's
/// spelling of it or a query.
#[test]
fn allowed_routes_forward_their_own_path() {
    block_on_local(async {
        let (_dir, anthropic, openai) = services();
        for (service, host, method, raw, want) in [
            (
                &anthropic,
                "platform.claude.com",
                "GET",
                "/V1//oauth/x/../hello?next=/v1/messages",
                "/v1/oauth/hello",
            ),
            (
                &openai,
                "auth.openai.com",
                "POST",
                "/api/accounts/deviceauth/usercode/?a=1",
                "/api/accounts/deviceauth/usercode",
            ),
            (
                &openai,
                "auth.openai.com",
                "POST",
                "/api/accounts/%64eviceauth/token;x",
                "/api/accounts/deviceauth/token",
            ),
        ] {
            let log = Log::default();
            service
                .send(
                    &Endpoint::new(host, 443),
                    request(method, raw, &[], "{}"),
                    &[],
                    upstream(&log, "application/json", r#"{"authorization_code":"c"}"#),
                )
                .await
                .unwrap();
            assert_eq!(log.borrow()[0].uri, want, "{raw}");
        }
    });
}

/// On the token hosts only the agents' routes are served: anything else
/// gets a local 403 and never goes upstream. The API hosts take every
/// path.
#[test]
fn other_token_host_routes_are_forbidden() {
    block_on_local(async {
        let (_dir, anthropic, openai) = services();
        let auth = Endpoint::new("auth.openai.com", 443);
        let platform = Endpoint::new("Platform.Claude.com.", 443);
        for (service, to, method, path) in [
            (&openai, &auth, "GET", "/"),
            (&openai, &auth, "GET", "/oauth/token"),
            (&openai, &auth, "POST", "/api/accounts/deviceauth/other"),
            (&openai, &auth, "GET", "/api/accounts/deviceauth/usercode"),
            (&openai, &auth, "POST", "/backend-api/codex/models"),
            (&anthropic, &platform, "GET", "/v1/oauth/token"),
            (&anthropic, &platform, "POST", "/v1/oauth/hello"),
            (&anthropic, &platform, "GET", "/v1/messages"),
            (&anthropic, &platform, "GET", "/oauth/code/callback"),
        ] {
            let log = Log::default();
            let resp = service
                .send(
                    to,
                    request(method, path, &[], ""),
                    &[],
                    upstream(&log, "text/plain", "ok"),
                )
                .await
                .unwrap();
            let (status, body) = body_of(resp).await;
            assert_eq!(status, 403, "{method} {path}: {body}");
            let error: Value = serde_json::from_str(&body).unwrap();
            assert_eq!(error["error"], "airlock_route_not_allowed", "{body}");
            assert!(log.borrow().is_empty(), "{method} {path}");
        }
        for (service, to, path) in [
            (&openai, Endpoint::new("chatgpt.com", 443), "/anything"),
            (
                &anthropic,
                Endpoint::new("api.anthropic.com", 443),
                "/anything",
            ),
        ] {
            let log = Log::default();
            let resp = service
                .send(
                    &to,
                    request("GET", path, &[], ""),
                    &[],
                    upstream(&log, "text/plain", "ok"),
                )
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{path}");
        }
    });
}

/// A grant of `service` with the real tokens and their surrogates.
async fn grant(store: &TokenStore, service: ServiceId, tokens: &[(TokenKind, &str, &str)]) {
    store
        .insert_grant(
            service,
            NewGrant {
                account_id: "acct".into(),
                account: None,
                organization: None,
                client_id: "client".into(),
                scopes: vec![],
                tokens: tokens
                    .iter()
                    .map(|(kind, real, surrogate)| Token {
                        kind: *kind,
                        real: (*real).into(),
                        surrogate: (*surrogate).into(),
                        expires_at: None,
                    })
                    .collect(),
            },
        )
        .await
        .unwrap();
}

/// On every path of an API host, a known access or API-key surrogate as
/// the whole `Authorization: Bearer` token or `x-api-key` value becomes
/// its real value. Surrogates in other headers and in the body stay as
/// they are. Other hosts get no swap.
#[test]
fn credential_surrogates_are_swapped_on_every_api_path_only() {
    block_on_local(async {
        let (_dir, store, anthropic, openai) = services_with_store();
        let access = "sk-ant-oat01-airlock-KNOWNACCESS";
        let key = "sk-ant-api03-airlock-KNOWNKEY";
        grant(
            &store,
            ServiceId::Anthropic,
            &[
                (TokenKind::Access, "sk-ant-oat01-REAL", access),
                (TokenKind::ApiKey, "sk-ant-api03-REALKEY", key),
            ],
        )
        .await;
        let api = Endpoint::new("api.anthropic.com", 443);
        for path in ["/v1/messages", "/api/oauth/claude_cli/roles", "/x/../y?z=1"] {
            let log = Log::default();
            let body = format!(r#"{{"token":"{access}"}}"#);
            let req = request(
                "POST",
                path,
                &[
                    ("authorization", &format!("Bearer {access}")),
                    ("x-api-key", key),
                    ("origin", &format!("https://{access}")),
                    ("anthropic-beta", access),
                ],
                &body,
            );
            let resp = anthropic
                .send(&api, req, &[], upstream(&log, "text/plain", "ok"))
                .await
                .unwrap();
            assert_eq!(resp.status(), 200, "{path}");
            let got = log.borrow()[0].clone();
            let header = |name: &str| got.headers.get(name).unwrap().to_str().unwrap().to_string();
            assert_eq!(
                header("authorization"),
                "Bearer sk-ant-oat01-REAL",
                "{path}"
            );
            assert_eq!(header("x-api-key"), "sk-ant-api03-REALKEY", "{path}");
            assert_eq!(header("origin"), format!("https://{access}"), "{path}");
            assert_eq!(header("anthropic-beta"), access, "{path}");
            assert!(got.headers.get("authorization").unwrap().is_sensitive());
            assert_eq!(got.body, body, "{path}: the body is never changed");
        }

        // A surrogate inside a credential value is no whole token.
        let log = Log::default();
        let req = request(
            "GET",
            "/v1/messages",
            &[("x-api-key", &format!("{key} extra"))],
            "",
        );
        let resp = anthropic
            .send(&api, req, &[], upstream(&log, "text/plain", "ok"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        assert!(log.borrow().is_empty());

        // The token host's allowed route: no swap.
        let log = Log::default();
        let req = request("GET", "/v1/oauth/hello", &[("x-custom", access)], "");
        anthropic
            .send(
                &Endpoint::new("platform.claude.com", 443),
                req,
                &[],
                upstream(&log, "text/plain", "ok"),
            )
            .await
            .unwrap();
        assert_eq!(log.borrow()[0].headers.get("x-custom").unwrap(), access);

        // Another service's API host: its surrogate is no credential there,
        // and it is not swapped in other headers.
        let chatgpt = Endpoint::new("chatgpt.com", 443);
        let log = Log::default();
        let req = request(
            "GET",
            "/backend-api/x",
            &[("authorization", &format!("Bearer {access}"))],
            "",
        );
        let resp = openai
            .send(&chatgpt, req, &[], upstream(&log, "text/plain", "ok"))
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
        assert!(log.borrow().is_empty());
        let req = request("GET", "/backend-api/x", &[("x-custom", access)], "");
        openai
            .send(&chatgpt, req, &[], upstream(&log, "text/plain", "ok"))
            .await
            .unwrap();
        assert_eq!(log.borrow()[0].headers.get("x-custom").unwrap(), access);
    });
}

/// A refresh or ID-token surrogate is no API credential: refused locally,
/// its real token never goes to the API host.
#[test]
fn refresh_and_id_surrogates_are_no_api_credentials() {
    block_on_local(async {
        let (_dir, store, anthropic, openai) = services_with_store();
        grant(
            &store,
            ServiceId::Anthropic,
            &[
                (
                    TokenKind::Access,
                    "sk-ant-oat01-REAL",
                    "sk-ant-oat01-airlock-A",
                ),
                (
                    TokenKind::Refresh,
                    "sk-ant-ort01-REAL",
                    "sk-ant-ort01-airlock-R",
                ),
            ],
        )
        .await;
        let fake_id = format!("{}e30.sig", crate::services::tokens::FAKE_JWT_PREFIX);
        grant(
            &store,
            ServiceId::Openai,
            &[
                (TokenKind::Access, "REAL-ACCESS-OPAQUE-1", "airlock-at-A"),
                (TokenKind::Id, "REAL-ID", &fake_id),
            ],
        )
        .await;
        for (service, to, name, value) in [
            (
                &anthropic,
                Endpoint::new("api.anthropic.com", 443),
                "authorization",
                "Bearer sk-ant-ort01-airlock-R".to_string(),
            ),
            (
                &anthropic,
                Endpoint::new("api.anthropic.com", 443),
                "x-api-key",
                "sk-ant-ort01-airlock-R".to_string(),
            ),
            (
                &openai,
                Endpoint::new("chatgpt.com", 443),
                "authorization",
                format!("Bearer {fake_id}"),
            ),
        ] {
            let log = Log::default();
            let req = request("GET", "/v1/messages", &[(name, &value)], "");
            let resp = service
                .send(&to, req, &[], upstream(&log, "text/plain", "ok"))
                .await
                .unwrap();
            let (status, body) = body_of(resp).await;
            assert_eq!(status, 401, "{value}: {body}");
            assert!(body.contains("airlock_foreign_credential"), "{body}");
            assert!(log.borrow().is_empty(), "{value}");
        }
    });
}

/// An upstream that answers with a body of `chunks`, streamed without a
/// length.
fn streaming(chunks: &[&str], content_type: &'static str) -> Next {
    let chunks: Vec<Bytes> = chunks.iter().map(|c| Bytes::from(c.to_string())).collect();
    Box::new(move |_req: Request<ResponseBody>| {
        Box::pin(async move {
            let stream = futures::stream::iter(
                chunks
                    .into_iter()
                    .map(|c| Ok::<_, crate::network::http::BoxError>(hyper::body::Frame::data(c))),
            );
            let body = http_body_util::StreamBody::new(stream).boxed_unsync();
            let mut resp = Response::new(Either::Left(Either::Right(body)));
            resp.headers_mut()
                .insert("content-type", content_type.parse().unwrap());
            Ok(resp)
        })
    })
}

/// What a guest reads of an answer until its end or an error, and whether
/// it ended with an error.
async fn read_until_error(resp: Response<ResponseBody>) -> (StatusCode, String, bool) {
    let status = resp.status();
    let mut body = resp.into_body();
    let mut read = Vec::new();
    while let Some(frame) = body.frame().await {
        match frame {
            Ok(frame) => read.extend_from_slice(&frame.into_data().unwrap_or_default()),
            Err(_) => return (status, String::from_utf8_lossy(&read).into_owned(), true),
        }
    }
    (status, String::from_utf8_lossy(&read).into_owned(), false)
}

/// A real Anthropic token of a realistic shape.
fn shaped(prefix: &str) -> String {
    format!("{prefix}-{}", "R".repeat(90))
}

/// Every API answer streams through the scan: a real token in the first
/// bytes of a JSON answer is a local 502, in a later chunk (or in an
/// answer that is no JSON) it ends the stream before the chunk; split
/// tokens are found; surrogates and large answers pass intact.
#[test]
fn api_answers_are_scanned_as_they_stream() {
    block_on_local(async {
        let (_dir, store, anthropic, _openai) = services_with_store();
        grant(
            &store,
            ServiceId::Anthropic,
            &[(
                TokenKind::Refresh,
                "sk-ant-ort01-REAL-REFRESH",
                "sk-ant-ort01-airlock-R",
            )],
        )
        .await;
        let api = Endpoint::new("api.anthropic.com", 443);
        let send = |next: Next| {
            let anthropic = anthropic.clone();
            let api = api.clone();
            async move {
                let resp = anthropic
                    .send(&api, request("GET", "/v1/messages", &[], ""), &[], next)
                    .await
                    .unwrap();
                read_until_error(resp).await
            }
        };
        // Chunked JSON without a length, a token shape split across chunks.
        let token = shaped("sk-ant-api03");
        let (x, y) = token.split_at(20);
        let (status, _, _) = send(streaming(
            &[&format!(r#"{{"key":"{x}"#), &format!(r#"{y}"}}"#)],
            "application/json",
        ))
        .await;
        assert_eq!(status, 502);
        // SSE: a known real token mid-stream ends the stream; its bytes
        // stay out. The status went on at once.
        let (status, read, failed) = send(streaming(
            &[
                "event: a\ndata: {}\n\n",
                "event: b\ndata: {\"t\":\"sk-ant-ort01-REAL-",
                "REFRESH\"}\n\n",
                "event: c\ndata: {}\n\n",
            ],
            "text/event-stream",
        ))
        .await;
        assert_eq!(status, 200);
        assert!(failed, "the stream ends with an error");
        assert_eq!(read, "event: a\ndata: {}\n\nevent: b\ndata: {\"t\":\"");
        // SSE: a token shape in model output passes (exact search only).
        let model = format!("data: {{\"text\":\"an example key: {token}\"}}\n\n");
        let (status, read, failed) = send(streaming(&[&model], "text/event-stream")).await;
        assert_eq!((status, failed, read), (StatusCode::OK, false, model));
        // A large streamed answer with surrogates passes intact.
        let line = format!(
            "data: {{\"k\":\"sk-ant-oat01-airlock-{}\"}}\n\n",
            "x".repeat(50)
        );
        let chunks: Vec<String> = (0..300).map(|_| line.clone()).collect();
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let (status, read, failed) = send(streaming(&refs, "text/event-stream")).await;
        assert_eq!((status, failed), (StatusCode::OK, false));
        assert_eq!(read, chunks.concat());
        // A JSON answer larger than the scan holds, the token at its end:
        // the stream ends before the token.
        let pad = "a ".repeat(50 * 1024);
        let (status, read, failed) = send(streaming(
            &[r#"{"pad":""#, &pad, &format!(r#"","k":"{token}"}}"#)],
            "application/json",
        ))
        .await;
        assert_eq!((status, failed), (StatusCode::OK, true));
        assert!(!read.contains("RRRR"));
        assert!(read.len() >= 100 * 1024);
        // A big JSON answer with a length passes intact.
        let big = format!(r#"{{"pad":"{}"}}"#, "a".repeat(100 * 1024));
        let log = Log::default();
        let (status, read, failed) = send(upstream(&log, "application/json", &big)).await;
        assert_eq!((status, failed, read), (StatusCode::OK, false, big));
    });
}

/// The real value of a masked secret injected into the request is
/// searched for in the answer, also in an event stream.
#[test]
fn an_injected_secret_is_searched_for_in_the_answer() {
    block_on_local(async {
        let (_dir, anthropic, _openai) = services();
        let api = Endpoint::new("api.anthropic.com", 443);
        let secret = crate::network::target::InjectedSecret::new(crate::project::MaskedSecret {
            name: "ANTHROPIC_API_KEY".into(),
            real: "user-real-api-key-123".into(),
            surrogate: "masked".into(),
        });
        let req = request(
            "GET",
            "/v1/messages",
            &[("x-api-key", "user-real-api-key-123")],
            "",
        );
        let resp = anthropic
            .send(
                &api,
                req,
                &[secret],
                streaming(&["data: user-real-api-key-123\n\n"], "text/event-stream"),
            )
            .await
            .unwrap();
        let (status, read, failed) = read_until_error(resp).await;
        assert_eq!((status, failed, read.as_str()), (StatusCode::OK, true, ""));

        // A value shorter than 16 characters is not searched for.
        let short = crate::network::target::InjectedSecret::new(crate::project::MaskedSecret {
            name: "SHORT".into(),
            real: "short-secret".into(),
            surrogate: "masked".into(),
        });
        let req = request("GET", "/v1/messages", &[("x-api-key", "short-secret")], "");
        let resp = anthropic
            .send(
                &api,
                req,
                &[short],
                streaming(&["data: short-secret\n\n"], "text/event-stream"),
            )
            .await
            .unwrap();
        let (status, read, failed) = read_until_error(resp).await;
        assert_eq!(
            (status, failed, read.as_str()),
            (StatusCode::OK, false, "data: short-secret\n\n")
        );
    });
}

/// A real token in a response header or trailer, or a compressed answer,
/// is refused.
#[test]
fn api_answer_headers_trailers_and_compression_are_refused() {
    block_on_local(async {
        let (_dir, anthropic, _openai) = services();
        let api = Endpoint::new("api.anthropic.com", 443);
        let token = shaped("sk-ant-oat01");
        for (name, value) in [
            ("x-echo", token.clone()),
            ("set-cookie", format!("s={token}; Path=/")),
            ("content-encoding", "gzip".into()),
            ("content-encoding", "br".into()),
        ] {
            let next: Next = Box::new(move |_req: Request<ResponseBody>| {
                Box::pin(async move {
                    let mut resp = Response::new(Either::Right(Full::new(Bytes::from("x"))));
                    resp.headers_mut().insert(name, value.parse().unwrap());
                    Ok(resp)
                })
            });
            let resp = anthropic
                .send(&api, request("GET", "/v1/messages", &[], ""), &[], next)
                .await
                .unwrap();
            assert_eq!(resp.status(), 502, "{name}");
        }
        // A trailer: the stream ends with an error.
        for content_type in ["application/json", "text/event-stream"] {
            let mut trailers = hyper::HeaderMap::new();
            trailers.insert("x-echo", token.parse().unwrap());
            let next: Next = Box::new(move |_req: Request<ResponseBody>| {
                Box::pin(async move {
                    let frames = vec![
                        Ok::<_, crate::network::http::BoxError>(hyper::body::Frame::data(
                            Bytes::from("data: ok\n\n"),
                        )),
                        Ok(hyper::body::Frame::trailers(trailers)),
                    ];
                    let body = http_body_util::StreamBody::new(futures::stream::iter(frames))
                        .boxed_unsync();
                    let mut resp = Response::new(Either::Left(Either::Right(body)));
                    resp.headers_mut()
                        .insert("content-type", content_type.parse().unwrap());
                    Ok(resp)
                })
            });
            let resp = anthropic
                .send(&api, request("GET", "/v1/messages", &[], ""), &[], next)
                .await
                .unwrap();
            let (status, _, failed) = read_until_error(resp).await;
            assert!(status == 502 || failed, "{content_type}");
        }
    });
}

/// API answers with a real token of the provider are refused; surrogates,
/// short examples and identifiers pass.
#[test]
fn api_answers_with_real_tokens_are_refused() {
    block_on_local(async {
        let (_dir, anthropic, _openai) = services();
        let api = Endpoint::new("api.anthropic.com", 443);
        let token = shaped("sk-ant-api03");
        for (content_type, answer, refused) in [
            ("application/json", format!(r#"{{"key":"{token}"}}"#), true),
            (
                "application/json",
                format!(r#"{{"t":["{}"]}}"#, shaped("sk-ant-ort01")),
                true,
            ),
            ("text/plain", shaped("sk-ant-oat01"), true),
            (
                "application/json",
                r#"{"key":"sk-ant-api03-airlock-x"}"#.into(),
                false,
            ),
            (
                "application/json",
                r#"{"key":"sk-ant-api03-REAL-short"}"#.into(),
                false,
            ),
            (
                "application/json",
                format!(r#"{{"id":"assert_{token}"}}"#),
                false,
            ),
            (
                "application/json",
                r#"{"access_token":"x","code":"c"}"#.into(),
                false,
            ),
        ] {
            let log = Log::default();
            let resp = anthropic
                .send(
                    &api,
                    request("GET", "/v1/models", &[], ""),
                    &[],
                    upstream(&log, content_type, &answer),
                )
                .await
                .unwrap();
            let (status, body, failed) = read_until_error(resp).await;
            assert_eq!(status == 502 || failed, refused, "{answer}: {body}");
            if !refused {
                assert_eq!(body, answer);
            }
        }
    });
}

/// On chatgpt.com's API paths only a surrogate or an injected secret
/// goes upstream.
#[test]
fn chatgpt_api_credentials_are_strict() {
    block_on_local(async {
        let (_dir, _anthropic, openai) = services();
        let chatgpt = Endpoint::new("chatgpt.com", 443);
        let secret = crate::network::target::InjectedSecret::new(crate::project::MaskedSecret {
            name: "CHATGPT_TOKEN".into(),
            real: "real-chatgpt-token".into(),
            surrogate: "masked".into(),
        });
        for (token, injected, status) in [
            ("real-chatgpt-token", vec![], 401),
            ("real-chatgpt-token", vec![secret.clone()], 200),
            ("airlock-at-unknown", vec![secret], 401),
        ] {
            let log = Log::default();
            let req = request(
                "GET",
                "/backend-api/codex/models",
                &[("authorization", &format!("Bearer {token}"))],
                "",
            );
            let resp = openai
                .send(&chatgpt, req, &injected, upstream(&log, "text/plain", "ok"))
                .await
                .unwrap();
            let (got, body) = body_of(resp).await;
            assert_eq!(got, status, "{token}: {body}");
            assert_eq!(log.borrow().len(), usize::from(status == 200), "{token}");
        }
    });
}

#[test]
fn the_foreign_credential_answer_names_the_opt_out() {
    block_on_local(async {
        let (_dir, anthropic, _openai) = services();
        let log = Log::default();
        let req = request(
            "GET",
            "/v1/messages",
            &[("x-api-key", "sk-ant-api03-REAL")],
            "",
        );
        let resp = anthropic
            .send(
                &Endpoint::new("api.anthropic.com", 443),
                req,
                &[],
                upstream(&log, "text/plain", "ok"),
            )
            .await
            .unwrap();
        let (status, body) = body_of(resp).await;
        assert_eq!(status, 401);
        let message: Value = serde_json::from_str(&body).unwrap();
        assert_eq!(
            message["error"]["message"],
            "airlock: with [network.services] anthropic enabled, credentials for \
             api.anthropic.com must come from the anthropic sign-in in the sandbox or from \
             a masked [env] secret with inject"
        );
    });
}
