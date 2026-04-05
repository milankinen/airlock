use axum::Router;
use axum::routing::{get, post};

use super::helpers::*;

#[test]
fn plain_http_get() {
    run_plain(|proxy| async move {
        let addr = serve(Router::new().route("/", get(|| async { "hello world" }))).await;
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .expect("should connect");
        let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
        assert!(resp.contains("200"), "expected 200, got: {resp}");
        assert!(resp.contains("hello world"), "expected body, got: {resp}");
    });
}

#[test]
fn host_not_allowed_is_denied() {
    run_network(
        vec!["example.com".into()],
        vec![],
        vec![],
        |proxy| async move {
            let conn = TestConnection::connect(&proxy, "127.0.0.1", 80).await;
            assert!(conn.is_none(), "connection should be denied");
        },
    );
}

#[test]
fn wildcard_host_allowed() {
    run_network(vec!["*.0.0.1".into()], vec![], vec![], |proxy| async move {
        let addr = serve(Router::new().route("/", get(|| async { "ok" }))).await;
        let conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port()).await;
        assert!(conn.is_some(), "127.0.0.1 should match *.0.0.1");
    });
}

#[test]
fn star_allows_everything() {
    run_plain(|proxy| async move {
        let conn = TestConnection::connect(&proxy, "anything.example.com", 80).await;
        assert!(conn.is_some(), "* should match everything");
    });
}

#[test]
fn empty_allowed_hosts_denies_all() {
    run_network(vec![], vec![], vec![], |proxy| async move {
        let conn = TestConnection::connect(&proxy, "127.0.0.1", 80).await;
        assert!(conn.is_none(), "empty allowed_hosts should deny everything");
    });
}

#[test]
fn post_with_body() {
    run_plain(|proxy| async move {
        let addr =
            serve(Router::new().route("/echo", post(|body: String| async move { body }))).await;
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .unwrap();
        let resp = conn
            .roundtrip(&http_post(addr.port(), "/echo", "test-body"))
            .await;
        assert!(resp.contains("200"), "expected 200, got: {resp}");
        assert!(
            resp.contains("test-body"),
            "expected echoed body, got: {resp}"
        );
    });
}

#[test]
fn large_response() {
    run_plain(|proxy| async move {
        let big = "x".repeat(100_000);
        let addr = serve(Router::new().route(
            "/big",
            get(move || {
                let big = big.clone();
                async move { big }
            }),
        ))
        .await;
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .unwrap();
        let resp = conn.roundtrip(&http_get(addr.port(), "/big")).await;
        assert!(
            resp.len() > 100_000,
            "expected 100KB+, got {} bytes",
            resp.len()
        );
    });
}
