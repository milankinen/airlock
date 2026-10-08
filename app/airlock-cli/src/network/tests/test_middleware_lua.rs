//! Tests for Lua middleware: scripts read and change requests and
//! responses, match hosts and deny requests.

use std::collections::HashMap;

use axum::Router;
use axum::http::StatusCode;
use axum::routing::{any, get};

use crate::test_cfg::network::*;
use crate::test_cfg::upstream::SeenLog;
use crate::test_cfg::*;

/// A config with the given Lua middleware scripts, in order.
fn middleware(scripts: Vec<(&'static str, &'static str)>) -> TestNetworkConfig {
    TestNetworkConfig {
        middleware_scripts: scripts,
        ..Default::default()
    }
}

/// An upstream that records each request in `seen` and answers `reply`.
fn recording_app(seen: &SeenLog, reply: &'static str) -> Router {
    let seen = seen.clone();
    Router::new().route(
        "/{*path}",
        any(move |req: axum::extract::Request| async move {
            seen.record(req).await;
            reply
        }),
    )
}

/// Test that request changes from a chain of Lua middlewares reach the
/// upstream. Scripts set headers, Basic auth and text or JSON bodies.
///   1. Add two middlewares that set headers, Basic auth and a body
///   2. Send a POST to a text path and to a JSON path
///   3. Check the headers and bodies that the upstream got
#[test]
fn middleware_request_edits_reach_upstream() {
    let cfg = middleware(vec![
        (
            "first",
            r#"
            req:setHeader("x-first", "1")
            req:setBasicAuth("alice", "s3cr:t")
            if req.path == "/json" then
                req:setBody({key = "value", num = 42})
            else
                req:setBody("replaced-by-lua")
            end
            "#,
        ),
        ("second", r#"req:setHeader("x-second", "2")"#),
    ]);
    run_with_config(cfg, |proxy, _, _| async move {
        let seen = SeenLog::default();
        let port = serve(recording_app(&seen, "upstream-reply")).await.port();
        for path in ["/text", "/json"] {
            let resp = TestConnection::local(&proxy, port)
                .await
                .roundtrip(&http_post(port, path, "original"))
                .await;
            assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
            assert!(resp.ends_with("upstream-reply"), "{resp}");
        }

        let [text, json] = seen.all().try_into().unwrap();
        for req in [&text, &json] {
            assert_eq!(req.header("x-first"), Some("1"));
            assert_eq!(req.header("x-second"), Some("2"));
            // Base64 of "alice:s3cr:t". The colon in the password must stay.
            assert_eq!(req.header("authorization"), Some("Basic YWxpY2U6czNjcjp0"));
        }
        assert_eq!(text.body, "replaced-by-lua");
        let json: serde_json::Value = serde_json::from_str(&json.body).unwrap();
        assert_eq!(json, serde_json::json!({"key": "value", "num": 42}));
    });
}

/// Test that a Lua middleware can read the request body and the response
/// body, as text and as JSON, and that the guest still gets the full body.
///   1. Add a middleware that logs the request body and response body data
///   2. Send a POST to an upstream that answers JSON
///   3. Check the response that the guest gets and the log messages
#[test]
fn middleware_reads_request_and_response_bodies() {
    let cfg = middleware(vec![(
        "read bodies",
        r#"
        log("request: " .. req:body():text())
        local res = req:send()
        local body = res:body()
        log("length: " .. #body)
        log("count: " .. tostring(body:json().count))
        "#,
    )]);
    run_with_config(cfg, |proxy, log, _| async move {
        let port = serve(Router::new().route(
            "/",
            any(|| async { ([("content-type", "application/json")], r#"{"count":5}"#) }),
        ))
        .await
        .port();
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&http_post(port, "/", "hello-body"))
            .await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.ends_with(r#"{"count":5}"#), "{resp}");
        assert_eq!(
            log.messages(),
            ["request: hello-body", "length: 11", "count: 5"]
        );
    });
}

/// Test that response changes from a Lua middleware reach the guest.
///   1. Add a middleware that changes the status, a header and the body
///   2. Send a GET through the proxy
///   3. Check the changed status, header and body
#[test]
fn middleware_response_edits_reach_guest() {
    let cfg = middleware(vec![(
        "edit response",
        r#"
        local res = req:send()
        res.status = 201
        res:setHeader("x-added", "by-lua")
        res:setBody("replaced-response")
        "#,
    )]);
    run_with_config(cfg, |proxy, _, _| async move {
        let port = serve(Router::new().route("/", get(|| async { "original" })))
            .await
            .port();
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&http_get(port, "/"))
            .await;
        assert!(resp.starts_with("HTTP/1.1 201"), "{resp}");
        assert!(resp.contains("x-added: by-lua"), "{resp}");
        assert!(resp.ends_with("\r\n\r\nreplaced-response"), "{resp}");
    });
}

/// Test that host matches in Lua use the connect target, not the `Host`
/// header. A guest controls the header, so a header match is easy to evade
/// or to spoof.
///   1. Add a middleware that denies `evil.com` and `127.0.0.1/target`
///   2. Send a request to 127.0.0.1 with a spoofed `Host: evil.com`
///   3. Check that the spoofed request passes
///   4. Check that `/target` gets 403 and other paths pass
#[test]
fn host_rule_matches_connect_target_not_host_header() {
    let cfg = middleware(vec![(
        "host rules",
        r#"
        if req:hostMatches("evil.com") then req:deny() end
        if req:hostMatches("127.0.0.1") and req.path == "/target" then req:deny() end
        "#,
    )]);
    run_with_config(cfg, |proxy, _, _| async move {
        let port = serve(Router::new().route("/{*path}", get(|| async { "ok" })))
            .await
            .port();
        let spoofed =
            format!("GET /spoof HTTP/1.1\r\nHost: evil.com:{port}\r\nConnection: close\r\n\r\n");
        for (request, status) in [
            (spoofed, "HTTP/1.1 200"),
            (http_get(port, "/target"), "HTTP/1.1 403"),
            (http_get(port, "/other"), "HTTP/1.1 200"),
        ] {
            let resp = TestConnection::local(&proxy, port)
                .await
                .roundtrip(&request)
                .await;
            assert!(resp.starts_with(status), "{request}: {resp}");
        }
    });
}

/// Test that the monitor response events mark a middleware deny, and keep
/// it apart from an upstream 403 and a policy deny. A deny after
/// `req:send()` must also drop the upstream body.
///   1. Add a middleware that denies `/denied` before send and `/late`
///      after send
///   2. Send requests that are allowed, denied by middleware, answered 403
///      by the upstream, and denied by policy
///   3. Check that no guest response contains the upstream body
///   4. Check the allowed, status and denied fields of each event
#[test]
fn middleware_deny_is_reported_on_response_event() {
    let cfg = TestNetworkConfig {
        allowed_hosts: vec!["127.0.0.1".into()],
        middleware_scripts: vec![(
            "deny /denied and /late",
            r#"
            if req.path == "/denied" then req:deny() end
            if req.path == "/late" then
                req:send()
                req:deny()
            end
            "#,
        )],
        ..Default::default()
    };
    run_with_events(cfg, |proxy, mut events| async move {
        let port = serve(
            Router::new()
                .route("/allowed", get(|| async { "ok" }))
                .route("/denied", get(|| async { "secret" }))
                .route("/late", get(|| async { "secret" }))
                .route(
                    "/forbidden",
                    get(|| async { (StatusCode::FORBIDDEN, "no") }),
                ),
        )
        .await
        .port();

        for (host, path) in [
            ("127.0.0.1", "/allowed"),
            ("127.0.0.1", "/denied"),
            ("127.0.0.1", "/late"),
            ("127.0.0.1", "/forbidden"),
            // Not in the allowed hosts, so the policy denies it.
            ("blocked.example", "/policy"),
        ] {
            let resp = TestConnection::connect(&proxy, host, port)
                .await
                .unwrap()
                .roundtrip(&http_get(port, path))
                .await;
            assert!(!resp.contains("secret"), "{path}: {resp}");
        }

        // Join each response event to its request event by id.
        let mut seen = HashMap::new();
        let mut paths = HashMap::new();
        while let Ok(ev) = events.try_recv() {
            match ev {
                airlock_monitor::NetworkEvent::Request(r) => {
                    paths.insert(r.id, (r.path.clone(), r.allowed));
                }
                airlock_monitor::NetworkEvent::Response(r) => {
                    let (path, allowed) = paths[&r.id].clone();
                    seen.insert(path, (allowed, r.status, r.denied));
                }
                _ => {}
            }
        }
        assert_eq!(seen["/allowed"], (true, 200, false));
        assert_eq!(seen["/denied"], (true, 403, true));
        assert_eq!(seen["/late"], (true, 403, true));
        assert_eq!(seen["/forbidden"], (true, 403, false));
        assert_eq!(seen["/policy"], (false, 403, false));
    });
}
