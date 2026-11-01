//! Remote Control on the Anthropic API host: the worker tokens that
//! Anthropic mints through `bridge` go upstream unchanged on the worker
//! routes of their own session, and the answers still get the scan.

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

/// Call `bridge` for `session` with the access surrogate. Check that the
/// guest gets the worker token `jwt`.
async fn mint(services: &ProductionServices, session: &str, jwt: &str) {
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
    assert_eq!(answer.status, 200, "{}", answer.body);
    assert_eq!(answer.json()["worker_jwt"], jwt);
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

/// Test that a worker token from `bridge` goes upstream unchanged on the
/// worker routes of its session, so that Remote Control can connect. A
/// new `bridge` call adds a token, and the old token still works for the
/// streams that are open. An injected secret also works for `bridge`.
///   1. Call `bridge` with the access surrogate (the answer spells the host
///      in mixed case) and check that the upstream gets the real access
///      token
///   2. Send the worker token on the worker routes of the session
///   3. Check that the upstream gets the same `Authorization` header, asks
///      for an uncompressed answer, and that the event stream gets to the
///      guest intact
///   4. Call `bridge` again for a new token and check that both tokens pass
///   5. Call `bridge` for another session with an injected secret, and
///      check that its token passes
#[test]
fn worker_token_from_bridge_passes_on_worker_routes_of_its_session() {
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
        assert_eq!(answer.status, 200, "{}", answer.body);
        assert_eq!(
            log.all()[0].header("authorization"),
            Some("Bearer sk-ant-oat01-REAL")
        );
        mint(&services, SESSION, JWT2).await;

        let events = "event: client_event\ndata: {\"type\":\"user\"}\n\n";
        for token in [JWT, JWT2] {
            let worker = format!("Bearer {token}");
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
                    [(worker.clone(), "identity".to_string())],
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
        assert_eq!(answer.status, 200, "{}", answer.body);
        let log = GotLog::default();
        let worker = format!("Bearer {JWT}");
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
        assert_eq!(log.all()[0].header("authorization"), Some(worker.as_str()));
    });
}

/// Test that the proxy refuses a worker token that it did not see
/// Anthropic mint for that session, or that is not the only credential.
/// Thus a guest cannot use a worker token of a session in another account.
///   1. Call `bridge` for a session
///   2. Send its token for another session id, also one that differs only
///      in case, and on routes that are no worker routes
///   3. Send a token that `bridge` did not give on a worker route
///   4. Send the token on paths that only normalize onto a worker route,
///      with `x-api-key`, and with a second `Authorization` header
///   5. Check that each request gets a local 401 with
///      `airlock_foreign_credential` and does not go upstream
#[test]
fn worker_token_that_bridge_did_not_give_for_session_is_refused() {
    block_on_local(async {
        let services = services_with_grant("sk-ant-oat01-REAL").await;
        mint(&services, SESSION, JWT).await;
        let jwt = format!("Bearer {JWT}");
        let other = format!("Bearer {JWT2}");
        let worker = format!("/v1/code/sessions/{SESSION}/worker");
        let bridge = format!("/v1/code/sessions/{SESSION}/bridge");
        let ingress = format!("/v1/session_ingress/session/{SESSION}");
        let dotdot = format!("/v1/messages/../code/sessions/{SESSION}/worker");
        let encoded = format!("/v1/messages/..%2fcode/sessions/{SESSION}/worker");
        let auth: &[(&str, &str)] = &[("authorization", &jwt)];
        let cases: &[(&str, &[(&str, &str)])] = &[
            ("/v1/code/sessions/cse_other/worker", auth),
            ("/v1/code/sessions/CSE_01ABC/worker", auth),
            ("/v1/messages", auth),
            (&bridge, auth),
            (&ingress, auth),
            (&worker, &[("authorization", &other)]),
            (&dotdot, auth),
            (&encoded, auth),
            (&worker, &[("authorization", &jwt), ("x-api-key", "k")]),
            (
                &worker,
                &[
                    ("authorization", &jwt),
                    ("authorization", "Bearer sk-ant-oat01-foreign"),
                ],
            ),
        ];
        for (path, headers) in cases {
            assert_foreign(&services, path, headers).await;
        }
    });
}

/// Test that the proxy keeps a worker token only from a `bridge` answer
/// for the API host and the user's own credential. A token that points
/// Claude Code to another host or that a request without a credential got
/// must not open the worker routes.
///   1. Call `bridge` with an answer whose `api_base_url` is another host
///      and check the local 502
///   2. Call `bridge` without a credential and check that the answer passes
///   3. Send both tokens on the worker route and check the local 401
#[test]
fn bridge_token_for_other_host_or_without_credential_is_not_kept() {
    block_on_local(async {
        let services = services_with_grant("sk-ant-oat01-REAL").await;
        let bearer = format!("Bearer {ACCESS}");
        let answer = bridge(
            &services,
            &GotLog::default(),
            SESSION,
            (&[("authorization", &bearer)], &[]),
            JWT,
            "https://api.example.com",
        )
        .await;
        assert_eq!(answer.status, 502, "{}", answer.body);
        assert_eq!(answer.json()["error"], "server_error");

        let answer = bridge(
            &services,
            &GotLog::default(),
            SESSION,
            (&[], &[]),
            JWT2,
            API_BASE,
        )
        .await;
        assert_eq!(answer.status, 200, "{}", answer.body);

        let worker = format!("/v1/code/sessions/{SESSION}/worker");
        for token in [JWT, JWT2] {
            let value = format!("Bearer {token}");
            assert_foreign(&services, &worker, &[("authorization", &value)]).await;
        }
    });
}

/// Test that the scan still checks the answers of `bridge` and of the
/// worker routes. An upstream must not send a real token of the store to
/// the guest on these routes either.
///   1. Store a grant with a real access token
///   2. Call `bridge` with an answer that has the real token and check the
///      local 502
///   3. Call `bridge` for a worker token, and send it on a worker route
///   4. Let the upstream stream the real token back, split over two chunks
///   5. Check that the answers are refused without the token
#[test]
fn answer_on_remote_control_route_with_real_store_token_is_refused() {
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

        mint(&services, SESSION, JWT).await;
        let (first, second) = real.split_at(30);
        let next = streaming(
            &[&format!("data: {first}"), &format!("{second}\n\n")],
            "text/event-stream",
        );
        let worker = format!("Bearer {JWT}");
        let path = format!("/v1/code/sessions/{SESSION}/worker/events/stream");
        let req = request("GET", &path, &[("authorization", &worker)], "");
        let answer = services
            .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
            .await;
        assert!(answer.refused(), "{}", answer.body);
        assert!(!answer.body.contains(&real), "{}", answer.body);
    });
}
