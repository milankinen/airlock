use airlock_common::supervisor_capnp::network_proxy;
use axum::Router;
use axum::routing::{get, post};

use super::helpers::*;

fn with_middleware<F, Fut>(scripts: Vec<(&'static str, &'static str)>, f: F)
where
    F: FnOnce(network_proxy::Client) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    run_network(vec!["*".into()], scripts, f);
}

fn with_middleware_log<F, Fut>(scripts: Vec<(&'static str, &'static str)>, f: F)
where
    F: FnOnce(network_proxy::Client, RequestLog) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    run_network_with_log(vec!["*".into()], scripts, f);
}

#[test]
fn deny_by_path() {
    with_middleware(
        vec![(
            "deny /denied",
            r#"if req.path == "/denied" then req:deny() end"#,
        )],
        |proxy| async move {
            let addr = serve(
                Router::new()
                    .route("/allowed", get(|| async { "ok" }))
                    .route("/denied", get(|| async { "secret" })),
            )
            .await;

            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/allowed")).await;
            assert!(resp.contains("200"), "allowed should pass: {resp}");

            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/denied")).await;
            assert!(resp.contains("403"), "denied should return 403: {resp}");
        },
    );
}

#[test]
fn deny_by_host() {
    with_middleware(
        vec![(
            "deny evil",
            r#"if req:hostMatches("evil.com") then req:deny() end"#,
        )],
        |proxy| async move {
            let addr = serve(Router::new().route("/", get(|| async { "ok" }))).await;

            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(resp.contains("200"), "normal host should pass: {resp}");

            // Connect to localhost but send Host: evil.com in the HTTP request
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn
                .roundtrip(&format!(
                    "GET / HTTP/1.1\r\nHost: evil.com:{}\r\nConnection: close\r\n\r\n",
                    addr.port()
                ))
                .await;
            assert!(resp.contains("403"), "evil.com should be denied: {resp}");
        },
    );
}

#[test]
fn inject_request_header() {
    with_middleware(
        vec![("inject", r#"req:setHeader("x-injected", "from-lua")"#)],
        |proxy| async move {
            let addr = serve(Router::new().route(
                "/",
                get(|headers: axum::http::HeaderMap| async move {
                    headers
                        .get("x-injected")
                        .map_or("missing".into(), |v| v.to_str().unwrap().to_string())
                }),
            ))
            .await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(
                resp.contains("from-lua"),
                "expected injected header: {resp}"
            );
        },
    );
}

#[test]
fn read_request_body() {
    with_middleware_log(
        vec![(
            "read body",
            r#"
            local b = req:body()
            log("body: " .. b:text())
        "#,
        )],
        |proxy, log| async move {
            let addr = serve(Router::new().route("/", post(|| async { "received" }))).await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn
                .roundtrip(&http_post(addr.port(), "/", "hello-body"))
                .await;
            assert!(resp.contains("200"), "should succeed: {resp}");
            assert!(
                log.messages()
                    .iter()
                    .any(|m| m.contains("body: hello-body")),
                "expected log with body, got: {:?}",
                log.messages()
            );
        },
    );
}

#[test]
fn replace_request_body() {
    with_middleware(
        vec![("replace", r#"req:setBody("replaced-by-lua")"#)],
        |proxy| async move {
            let addr =
                serve(Router::new().route("/echo", post(|body: String| async move { body }))).await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn
                .roundtrip(&http_post(addr.port(), "/echo", "original"))
                .await;
            assert!(
                resp.contains("replaced-by-lua"),
                "body should be replaced: {resp}"
            );
        },
    );
}

#[test]
fn set_json_body() {
    with_middleware(
        vec![("json", r#"req:setBody({key = "value", num = 42})"#)],
        |proxy| async move {
            let addr =
                serve(Router::new().route("/echo", post(|body: String| async move { body }))).await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn
                .roundtrip(&http_post(addr.port(), "/echo", "ignored"))
                .await;
            assert!(resp.contains("key"), "should contain JSON key: {resp}");
            assert!(resp.contains("42"), "should contain number: {resp}");
        },
    );
}

#[test]
fn explicit_send_and_read_response() {
    with_middleware_log(
        vec![(
            "read resp",
            r#"
            local res = req:send()
            local body = res:body()
            log("resp: " .. body:text())
        "#,
        )],
        |proxy, log| async move {
            let addr = serve(Router::new().route("/", get(|| async { "server-response" }))).await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(
                resp.contains("server-response"),
                "response preserved: {resp}"
            );
            assert!(
                log.messages()
                    .iter()
                    .any(|m| m.contains("resp: server-response")),
                "expected log with response body, got: {:?}",
                log.messages()
            );
        },
    );
}

#[test]
fn modify_response_status() {
    with_middleware(
        vec![(
            "status",
            r"
            local res = req:send()
            res.status = 201
        ",
        )],
        |proxy| async move {
            let addr = serve(Router::new().route("/", get(|| async { "ok" }))).await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(resp.contains("201"), "expected 201: {resp}");
        },
    );
}

#[test]
fn modify_response_header() {
    with_middleware(
        vec![(
            "resp header",
            r#"
            local res = req:send()
            res:setHeader("x-added", "by-lua")
        "#,
        )],
        |proxy| async move {
            let addr = serve(Router::new().route("/", get(|| async { "ok" }))).await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(resp.contains("x-added: by-lua"), "expected header: {resp}");
        },
    );
}

#[test]
fn replace_response_body() {
    with_middleware(
        vec![(
            "replace resp",
            r#"
            local res = req:send()
            res:setBody("replaced-response")
        "#,
        )],
        |proxy| async move {
            let addr = serve(Router::new().route("/", get(|| async { "original" }))).await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(
                resp.contains("replaced-response"),
                "expected replaced body: {resp}"
            );
        },
    );
}

#[test]
fn implicit_send() {
    with_middleware(
        vec![("no-op", r#"req:setHeader("x-touched", "yes")"#)],
        |proxy| async move {
            let addr = serve(Router::new().route("/", get(|| async { "implicit" }))).await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(resp.contains("200"), "implicit send should succeed: {resp}");
            assert!(
                resp.contains("implicit"),
                "body should pass through: {resp}"
            );
        },
    );
}

#[test]
fn multiple_middleware_layers() {
    with_middleware(
        vec![
            ("first", r#"req:setHeader("x-first", "1")"#),
            ("second", r#"req:setHeader("x-second", "2")"#),
        ],
        |proxy| async move {
            let addr = serve(Router::new().route(
                "/",
                get(|headers: axum::http::HeaderMap| async move {
                    let a = headers.get("x-first").map_or("", |v| v.to_str().unwrap());
                    let b = headers.get("x-second").map_or("", |v| v.to_str().unwrap());
                    format!("first={a},second={b}")
                }),
            ))
            .await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(resp.contains("first=1"), "first header: {resp}");
            assert!(resp.contains("second=2"), "second header: {resp}");
        },
    );
}

#[test]
fn json_response_body() {
    with_middleware_log(
        vec![(
            "json resp",
            r#"
            local res = req:send()
            local data = res:body():json()
            log("count = " .. tostring(data.count))
        "#,
        )],
        |proxy, log| async move {
            let addr = serve(Router::new().route(
                "/",
                get(|| async { axum::Json(serde_json::json!({"status": "ok", "count": 5})) }),
            ))
            .await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(resp.contains("200"), "should succeed: {resp}");
            assert!(
                log.messages().iter().any(|m| m.contains("count = 5")),
                "expected JSON count log, got: {:?}",
                log.messages()
            );
        },
    );
}

#[test]
fn body_len() {
    with_middleware(
        vec![(
            "len check",
            r#"
            local res = req:send()
            local b = res:body()
            if #b ~= 5 then error("expected len 5, got " .. #b) end
        "#,
        )],
        |proxy| async move {
            let addr = serve(Router::new().route("/", get(|| async { "12345" }))).await;
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
                .await
                .unwrap();
            let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
            assert!(resp.contains("200"), "len check should pass: {resp}");
        },
    );
}
