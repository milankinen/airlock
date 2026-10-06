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
use crate::services::{anthropic, openai};

/// What the upstream got: method, URI, `Host`.
#[derive(Clone, Debug)]
struct Got {
    uri: String,
    host: Option<String>,
}

type Log = Rc<RefCell<Vec<Got>>>;

/// An upstream that records each request and answers `status`, `content
/// type` and `body` (with its length).
fn upstream(log: &Log, content_type: &'static str, body: &str) -> Next {
    let (log, body) = (log.clone(), body.to_string());
    Box::new(move |req: Request<ResponseBody>| {
        Box::pin(async move {
            log.borrow_mut().push(Got {
                uri: req.uri().to_string(),
                host: req
                    .headers()
                    .get("host")
                    .map(|h| h.to_str().unwrap().to_string()),
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
        store,
        tls,
        codes,
    ));
    (dir, anthropic, openai)
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

/// Every owned path that is not the API's passes the backstop: tokens and
/// codes in a JSON answer are refused, a surrogate code passes.
#[test]
fn non_api_paths_pass_the_backstop() {
    block_on_local(async {
        let (_dir, _anthropic, openai) = services();
        let chatgpt = Endpoint::new("chatgpt.com", 443);
        for (answer, refused) in [
            (json!({ "accessToken": "x", "access_token": "real" }), true),
            (json!({ "code": "real-code" }), true),
            (json!({ "authorization_code": "real-code" }), true),
            (json!({ "data": { "code_verifier": "v" } }), true),
            (json!({ "code": "airlock-code-x" }), false),
            (json!({ "user": "u" }), false),
        ] {
            let log = Log::default();
            let resp = openai
                .send(
                    &chatgpt,
                    request("GET", "/api/auth/session", &[], ""),
                    &[],
                    upstream(&log, "application/json", &answer.to_string()),
                )
                .await
                .unwrap();
            let (status, _) = body_of(resp).await;
            assert_eq!(status == 502, refused, "{answer}");
        }
    });
}

/// API answers: a small uncompressed JSON answer with a real token in a
/// provider format is refused; surrogates and streams pass.
#[test]
fn api_answers_pass_the_api_backstop() {
    block_on_local(async {
        let (_dir, anthropic, _openai) = services();
        let api = Endpoint::new("api.anthropic.com", 443);
        for (content_type, answer, refused) in [
            ("application/json", r#"{"key":"sk-ant-api03-REAL"}"#, true),
            ("application/json", r#"{"t":["sk-ant-ort01-REAL"]}"#, true),
            (
                "application/json",
                r#"{"key":"sk-ant-api03-airlock-x"}"#,
                false,
            ),
            (
                "application/json",
                r#"{"access_token":"x","code":"c"}"#,
                false,
            ),
            ("text/event-stream", "data: sk-ant-api03-REAL\n\n", false),
        ] {
            let log = Log::default();
            let resp = anthropic
                .send(
                    &api,
                    request("GET", "/v1/models", &[], ""),
                    &[],
                    upstream(&log, content_type, answer),
                )
                .await
                .unwrap();
            let (status, body) = body_of(resp).await;
            assert_eq!(status == 502, refused, "{answer}: {body}");
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
