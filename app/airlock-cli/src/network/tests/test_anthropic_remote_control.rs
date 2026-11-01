//! Remote Control on the Anthropic API host: the guest gets surrogates for
//! the worker tokens that Anthropic mints through `bridge`. A surrogate
//! goes upstream as the real token only on the worker routes of its own
//! session, and the answers still get the scan.

use std::cell::RefCell;
use std::rc::Rc;

use crate::network::interceptor::Next;
use crate::network::target::InjectedSecret;
use crate::services::ServiceId;
use crate::services::tokens::TokenKind;
use crate::test_cfg::block_on_local;
use crate::test_cfg::services::{
    Answer, GotLog, ProductionServices, answering, insert_grant, masked, production_services,
    request, shaped_token, streaming,
};

/// Access token surrogate of the stored grant.
const ACCESS: &str = "sk-ant-oat01-airlock-A";
/// A worker token as `bridge` gives it (`worker_jwt`).
const JWT: &str = "eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiJjc2VfMSJ9.c2lnbmF0dXJl";
/// A second worker token, from a new `bridge` call.
const JWT2: &str = "sk-ant-si-eyJhbGciOiJFUzI1NiJ9.eyJzdWIiOiIyIn0.c2lnMg";
/// The `api_base_url` of a `bridge` answer from the production API host.
const API_BASE: &str = "https://api.anthropic.com";
/// A session id as Claude Code sends it, with uppercase letters.
const SESSION: &str = "cse_01AbC";

/// Make the production services with an Anthropic grant whose access
/// surrogate is [`ACCESS`] and whose real access token is `real`.
async fn services_with_grant(real: &str) -> ProductionServices {
    let services = production_services();
    insert_grant(
        &services.store,
        ServiceId::Anthropic,
        &[(TokenKind::Access, real, ACCESS)],
    )
    .await;
    services
}

/// Send `POST /v1/code/sessions/{session}/bridge` with the `credential`
/// headers and the `injected` secrets to the API host. The upstream
/// answers with the worker token `jwt` and the base URL `base`, and
/// records the request in `log`.
async fn bridge(
    services: &ProductionServices,
    log: &GotLog,
    session: &str,
    (credential, injected): (&[(&str, &str)], &[InjectedSecret]),
    jwt: &str,
    base: &str,
) -> Answer {
    let body = format!(
        r#"{{"worker_jwt":"{jwt}","expires_in":3600,"api_base_url":"{base}","worker_epoch":1}}"#
    );
    let path = format!("/v1/code/sessions/{session}/bridge");
    let req = request("POST", &path, credential, "{}");
    let next = answering(log, "application/json", &body);
    services
        .send(
            ServiceId::Anthropic,
            "api.anthropic.com",
            req,
            injected,
            next,
        )
        .await
}

/// Send `POST /v1/code/sessions/{SESSION}/bridge` with the access
/// surrogate to the API host. The upstream answers `status` with the JSON
/// `body`.
async fn bridge_answering(services: &ProductionServices, status: u16, body: &str) -> Answer {
    let bearer = format!("Bearer {ACCESS}");
    let path = format!("/v1/code/sessions/{SESSION}/bridge");
    let req = request("POST", &path, &[("authorization", &bearer)], "{}");
    let body = body.to_string();
    let next: Next = Box::new(move |_req| {
        Box::pin(async move {
            let resp = hyper::Response::builder()
                .status(status)
                .header("content-type", "application/json")
                .body(http_body_util::Either::Right(body.into()))
                .unwrap();
            Ok(resp)
        })
    });
    services
        .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
        .await
}

/// Get the worker token surrogate of a `bridge` answer for the real token
/// `jwt`. Check that it keeps the prefix, header and claims of `jwt`, and
/// that only the signature is different.
fn surrogate_of(answer: &Answer, jwt: &str) -> String {
    assert_eq!(answer.status, 200, "{}", answer.body);
    let surrogate = answer.json()["worker_jwt"].as_str().unwrap().to_string();
    let (kept, signature) = surrogate.rsplit_once('.').unwrap();
    let (real_kept, real_signature) = jwt.rsplit_once('.').unwrap();
    assert_eq!(kept, real_kept);
    assert_ne!(signature, real_signature);
    surrogate
}

/// Call `bridge` for `session` with the access surrogate, for the real
/// worker token `jwt`.
/// Returns:
///   The worker token surrogate that the guest gets.
async fn mint(services: &ProductionServices, session: &str, jwt: &str) -> String {
    let bearer = format!("Bearer {ACCESS}");
    let answer = bridge(
        services,
        &GotLog::default(),
        session,
        (&[("authorization", &bearer)], &[]),
        jwt,
        API_BASE,
    )
    .await;
    surrogate_of(&answer, jwt)
}

/// Send `GET path` with `credential` as the bearer token to the API host.
/// The upstream streams `chunks` as an event stream.
async fn get_stream(
    services: &ProductionServices,
    path: &str,
    credential: &str,
    chunks: &[&str],
) -> Answer {
    let bearer = format!("Bearer {credential}");
    let req = request("GET", path, &[("authorization", &bearer)], "");
    let next = streaming(chunks, "text/event-stream");
    services
        .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
        .await
}

/// An upstream that keeps the `authorization` and `accept-encoding` of
/// each request in `seen` and streams `chunks` as an event stream.
fn recording_stream(seen: &Rc<RefCell<Vec<(String, String)>>>, chunks: &[&str]) -> Next {
    let seen = seen.clone();
    let stream = streaming(chunks, "text/event-stream");
    Box::new(move |req| {
        let header = |name| {
            req.headers()
                .get(name)
                .map(|v| v.to_str().unwrap().to_string())
                .unwrap_or_default()
        };
        seen.borrow_mut()
            .push((header("authorization"), header("accept-encoding")));
        stream(req)
    })
}

/// Send `POST path` with `headers` to the production Anthropic API host.
/// Check that it gets a local 401 with `airlock_foreign_credential` and
/// does not go upstream.
async fn assert_foreign(services: &ProductionServices, path: &str, headers: &[(&str, &str)]) {
    let log = GotLog::default();
    let req = request("POST", path, headers, "");
    let next = answering(&log, "text/plain", "ok");
    let answer = services
        .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
        .await;
    assert_eq!(answer.status, 401, "{path} {headers:?}: {}", answer.body);
    assert!(
        answer.body.contains("airlock_foreign_credential"),
        "{path} {headers:?}: {}",
        answer.body
    );
    assert!(log.is_empty(), "{path} {headers:?}");
}

/// Test that the guest gets a surrogate for each worker token from
/// `bridge`, and that the surrogate goes upstream as the real token on the
/// worker routes of its session. The real token must not get into the
/// sandbox. A new `bridge` call adds a token, and the old token still
/// works for the streams that are open. An injected secret also works for
/// `bridge`.
///   1. Call `bridge` with the access surrogate (the answer spells the host
///      in mixed case) and check that the upstream gets the real access
///      token and that the guest gets a surrogate
///   2. Call `bridge` again for a new token
///   3. Send both surrogates on the worker routes of the session
///   4. Check that the upstream gets the real tokens, asks for an
///      uncompressed answer, and that the event stream gets to the guest
///      intact
///   5. Call `bridge` for another session with an injected secret, and
///      check that its surrogate goes upstream as the real token
#[test]
fn worker_token_surrogate_from_bridge_goes_upstream_as_real_token() {
    block_on_local(async {
        let services = services_with_grant("sk-ant-oat01-REAL").await;
        let log = GotLog::default();
        let bearer = format!("Bearer {ACCESS}");
        let answer = bridge(
            &services,
            &log,
            SESSION,
            (&[("authorization", &bearer)], &[]),
            JWT,
            "https://API.Anthropic.com/",
        )
        .await;
        let first = surrogate_of(&answer, JWT);
        assert_eq!(
            log.all()[0].header("authorization"),
            Some("Bearer sk-ant-oat01-REAL")
        );
        let second = mint(&services, SESSION, JWT2).await;

        let events = "event: client_event\ndata: {\"type\":\"user\"}\n\n";
        for (surrogate, real) in [(&first, JWT), (&second, JWT2)] {
            let worker = format!("Bearer {surrogate}");
            for (method, route) in [
                ("GET", "/worker/events/stream"),
                ("PUT", "/worker"),
                ("POST", "/worker/heartbeat"),
            ] {
                let path = format!("/v1/code/sessions/{SESSION}{route}");
                let seen = Rc::new(RefCell::new(Vec::new()));
                let req = request(
                    method,
                    &path,
                    &[("authorization", &worker), ("accept-encoding", "gzip")],
                    "",
                );
                let next = recording_stream(&seen, &[events]);
                let answer = services
                    .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
                    .await;
                assert_eq!(answer.status, 200, "{path}: {}", answer.body);
                assert!(!answer.refused(), "{path}");
                assert_eq!(answer.body, events, "{path}");
                assert_eq!(
                    seen.borrow().as_slice(),
                    [(format!("Bearer {real}"), "identity".to_string())],
                    "{path}"
                );
            }
        }

        let secret = "real-user-oauth-token-1234";
        let injected = [InjectedSecret::new(masked(
            "CLAUDE_CODE_OAUTH_TOKEN",
            secret,
            "masked",
        ))];
        let bearer = format!("Bearer {secret}");
        let answer = bridge(
            &services,
            &GotLog::default(),
            "cse_02",
            (&[("authorization", &bearer)], &injected),
            JWT,
            API_BASE,
        )
        .await;
        let worker = format!("Bearer {}", surrogate_of(&answer, JWT));
        let log = GotLog::default();
        let req = request(
            "POST",
            "/v1/code/sessions/cse_02/worker/events",
            &[("authorization", &worker)],
            "",
        );
        let next = answering(&log, "application/json", "{}");
        let answer = services
            .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
            .await;
        assert_eq!(answer.status, 200, "{}", answer.body);
        let real = format!("Bearer {JWT}");
        assert_eq!(log.all()[0].header("authorization"), Some(real.as_str()));
    });
}

/// Test that the proxy refuses a worker token that is not a surrogate that
/// it gave for that session, or that is not the only credential. Thus a
/// guest cannot use a worker token of a session in another account, and
/// the real token is no credential.
///   1. Call `bridge` for a session
///   2. Send its surrogate for another session id, also one that differs
///      only in case, and on routes that are no worker routes
///   3. Send the real token and a token that `bridge` did not give on a
///      worker route
///   4. Send the surrogate on paths that only normalize onto a worker
///      route, with `x-api-key`, and with a second `Authorization` header
///   5. Check that each request gets a local 401 with
///      `airlock_foreign_credential` and does not go upstream
#[test]
fn worker_token_that_bridge_did_not_give_for_session_is_refused() {
    block_on_local(async {
        let services = services_with_grant("sk-ant-oat01-REAL").await;
        let surrogate = format!("Bearer {}", mint(&services, SESSION, JWT).await);
        let real = format!("Bearer {JWT}");
        let other = format!("Bearer {JWT2}");
        let worker = format!("/v1/code/sessions/{SESSION}/worker");
        let bridge = format!("/v1/code/sessions/{SESSION}/bridge");
        let ingress = format!("/v1/session_ingress/session/{SESSION}");
        let dotdot = format!("/v1/messages/../code/sessions/{SESSION}/worker");
        let encoded = format!("/v1/messages/..%2fcode/sessions/{SESSION}/worker");
        let auth: &[(&str, &str)] = &[("authorization", &surrogate)];
        let cases: &[(&str, &[(&str, &str)])] = &[
            ("/v1/code/sessions/cse_other/worker", auth),
            ("/v1/code/sessions/CSE_01ABC/worker", auth),
            ("/v1/messages", auth),
            (&bridge, auth),
            (&ingress, auth),
            (&worker, &[("authorization", &real)]),
            (&worker, &[("authorization", &other)]),
            (&dotdot, auth),
            (&encoded, auth),
            (
                &worker,
                &[("authorization", &surrogate), ("x-api-key", "k")],
            ),
            (
                &worker,
                &[
                    ("authorization", &surrogate),
                    ("authorization", "Bearer sk-ant-oat01-foreign"),
                ],
            ),
        ];
        for (path, headers) in cases {
            assert_foreign(&services, path, headers).await;
        }
    });
}

/// Test that the proxy refuses a `bridge` answer whose worker token it
/// cannot keep. A token that points Claude Code to another host, that a
/// request without a credential got, or that is no JWT must not get into
/// the sandbox.
///   1. Call `bridge` with an answer whose `api_base_url` is another host
///   2. Call `bridge` without a credential
///   3. Call `bridge` with an answer whose `worker_jwt` is no JWT
///   4. Check that each answer is a local 502 without the token
#[test]
fn bridge_answer_with_worker_token_that_cannot_be_kept_is_refused() {
    block_on_local(async {
        let services = services_with_grant("sk-ant-oat01-REAL").await;
        let bearer = format!("Bearer {ACCESS}");
        let surrogate: &[(&str, &str)] = &[("authorization", &bearer)];
        for (credential, jwt, base) in [
            (surrogate, JWT, "https://api.example.com"),
            (&[], JWT, API_BASE),
            (surrogate, "opaque-worker-token", API_BASE),
        ] {
            let answer = bridge(
                &services,
                &GotLog::default(),
                SESSION,
                (credential, &[]),
                jwt,
                base,
            )
            .await;
            assert_eq!(answer.status, 502, "{jwt} {base}: {}", answer.body);
            assert_eq!(answer.json()["error"], "server_error");
            assert!(!answer.body.contains(jwt), "{}", answer.body);
        }
    });
}

/// Test that the guest does not get a real worker token from any `bridge`
/// answer, also not from a copy in another field, a malformed token, or an
/// answer without a new token. The final scan searches each `bridge`
/// answer for all kept real worker tokens.
///   1. Call `bridge` for a worker token, so that the service keeps it
///   2. Let `bridge` answers copy a new token into another field and into
///      a nested field
///   3. Let a `bridge` answer have a `worker_jwt` that is no string
///   4. Let an error answer and a 2xx answer without `worker_jwt` copy the
///      kept token
///   5. Check that each answer is a local 502 without the token
#[test]
fn bridge_answer_with_real_worker_token_copy_or_malformed_token_is_refused() {
    block_on_local(async {
        let services = services_with_grant("sk-ant-oat01-REAL").await;
        mint(&services, SESSION, JWT).await;
        let fields = format!(r#""expires_in":3600,"api_base_url":"{API_BASE}","worker_epoch":1"#);
        for (status, body, token) in [
            (
                200,
                format!(r#"{{"worker_jwt":"{JWT2}","copy":"{JWT2}",{fields}}}"#),
                JWT2,
            ),
            (
                200,
                format!(r#"{{"worker_jwt":"{JWT2}","a":{{"b":["{JWT2}"]}},{fields}}}"#),
                JWT2,
            ),
            (
                200,
                format!(r#"{{"worker_jwt":{{"token":"{JWT2}"}},{fields}}}"#),
                JWT2,
            ),
            (400, format!(r#"{{"error":"{JWT}"}}"#), JWT),
            (200, format!(r#"{{"echo":"{JWT}"}}"#), JWT),
        ] {
            let answer = bridge_answering(&services, status, &body).await;
            assert_eq!(answer.status, 502, "{body}: {}", answer.body);
            assert_eq!(answer.json()["error"], "server_error");
            assert!(!answer.body.contains(token), "{}", answer.body);
        }
    });
}

/// Test that the scan checks the answers of `bridge` and of all API routes
/// for the real tokens of the store and the real worker tokens. An
/// upstream must not send them to the guest.
///   1. Store a grant with a real access token
///   2. Call `bridge` with an answer that has the real access token and
///      check the local 502
///   3. Call `bridge` for a worker token
///   4. Let the upstream of a worker route stream the real access token,
///      split over two chunks, and check that the answer is refused
///   5. Let the upstreams of a worker route and of `/v1/messages` stream
///      the real worker token, and check that the answers are refused
#[test]
fn answer_with_real_store_or_worker_token_is_refused() {
    block_on_local(async {
        let real = shaped_token("sk-ant-oat01");
        let services = services_with_grant(&real).await;
        let bearer = format!("Bearer {ACCESS}");
        let answer = bridge(
            &services,
            &GotLog::default(),
            SESSION,
            (&[("authorization", &bearer)], &[]),
            &real,
            API_BASE,
        )
        .await;
        assert!(answer.refused(), "{}", answer.body);
        assert!(!answer.body.contains(&real), "{}", answer.body);

        let surrogate = mint(&services, SESSION, JWT).await;
        let worker = format!("/v1/code/sessions/{SESSION}/worker/events/stream");
        let (first, second) = real.split_at(30);
        let chunks = [&format!("data: {first}"), &format!("{second}\n\n")];
        let answer = get_stream(&services, &worker, &surrogate, &chunks.map(String::as_str)).await;
        assert!(answer.refused(), "{}", answer.body);
        assert!(!answer.body.contains(&real), "{}", answer.body);

        let echo = format!("data: {JWT}\n\n");
        for (path, credential) in [
            (worker.as_str(), surrogate.as_str()),
            ("/v1/messages", ACCESS),
        ] {
            let answer = get_stream(&services, path, credential, &[&echo]).await;
            assert!(answer.refused(), "{path}: {}", answer.body);
            assert!(!answer.body.contains(JWT), "{path}: {}", answer.body);
        }
    });
}
