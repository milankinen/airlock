use airlock_common::network_capnp::network_proxy;
use axum::Router;
use axum::extract::Path;
use axum::routing::{get, post};

use super::helpers::*;

fn with_noop_middleware<F, Fut>(f: F)
where
    F: FnOnce(network_proxy::Client) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    run_network(
        vec!["*".into()],
        vec![("noop", "-- triggers HTTP detection")],
        f,
    );
}

#[test]
fn http_detection_with_middleware() {
    with_noop_middleware(|proxy| async move {
        let addr = serve(Router::new().route(
            "/{*path}",
            get(|Path(p): Path<String>| async move { format!("path={p}") }),
        ))
        .await;
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .unwrap();
        let resp = conn.roundtrip(&http_get(addr.port(), "/test-path")).await;
        assert!(resp.contains("200"), "expected 200, got: {resp}");
        assert!(
            resp.contains("path=test-path"),
            "expected path echo, got: {resp}"
        );
    });
}

#[test]
fn http_without_middleware_raw_relay() {
    run_plain(|proxy| async move {
        let addr = serve(Router::new().route("/", get(|| async { "raw-relay" }))).await;
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .unwrap();
        let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
        assert!(resp.contains("raw-relay"), "expected body, got: {resp}");
    });
}

#[test]
fn http_post_through_middleware() {
    with_noop_middleware(|proxy| async move {
        let addr =
            serve(Router::new().route("/echo", post(|body: String| async move { body }))).await;
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .unwrap();
        let resp = conn
            .roundtrip(&http_post(addr.port(), "/echo", "payload"))
            .await;
        assert!(resp.contains("200"), "expected 200, got: {resp}");
        assert!(
            resp.contains("payload"),
            "expected echoed body, got: {resp}"
        );
    });
}

#[test]
fn http_preserves_status_codes() {
    with_noop_middleware(|proxy| async move {
        let addr = serve(Router::new().route(
            "/not-found",
            get(|| async { (axum::http::StatusCode::NOT_FOUND, "nope") }),
        ))
        .await;
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .unwrap();
        let resp = conn.roundtrip(&http_get(addr.port(), "/not-found")).await;
        assert!(resp.contains("404"), "expected 404, got: {resp}");
    });
}

#[test]
fn http_preserves_response_headers() {
    with_noop_middleware(|proxy| async move {
        let addr =
            serve(Router::new().route("/", get(|| async { ([("x-custom", "test-value")], "ok") })))
                .await;
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .unwrap();
        let resp = conn.roundtrip(&http_get(addr.port(), "/")).await;
        assert!(
            resp.contains("x-custom: test-value"),
            "expected custom header, got: {resp}"
        );
    });
}

#[test]
fn http_keepalive_multiple_requests() {
    with_noop_middleware(|proxy| async move {
        let addr = serve(Router::new().route(
            "/{*path}",
            get(|Path(p): Path<String>| async move { format!("path={p}") }),
        ))
        .await;

        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .unwrap();

        // First request without Connection: close (keep-alive by default in HTTP/1.1)
        conn.send(http_get_keepalive(addr.port(), "/first").as_bytes())
            .await;
        let resp = conn.recv(500).await;
        assert!(resp.contains("200"), "first request: {resp}");
        assert!(resp.contains("path=first"), "first body: {resp}");

        // Second request on the same connection
        conn.send(http_get_keepalive(addr.port(), "/second").as_bytes())
            .await;
        let resp = conn.recv(500).await;
        assert!(resp.contains("200"), "second request: {resp}");
        assert!(resp.contains("path=second"), "second body: {resp}");
    });
}

/// When the upstream server closes the connection, the relay should
/// propagate the close to the guest so it can reconnect naturally
/// instead of getting 502 "operation was canceled" on subsequent requests.
#[test]
fn http_upstream_close_propagates_to_guest() {
    with_noop_middleware(|proxy| async move {
        let (addr, shutdown_tx) =
            serve_with_shutdown(Router::new().route("/", get(|| async { "hello" }))).await;

        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", addr.port())
            .await
            .unwrap();

        // First request succeeds
        conn.send(http_get_keepalive(addr.port(), "/").as_bytes())
            .await;
        let resp = conn.recv(500).await;
        assert!(
            resp.contains("200"),
            "initial request should succeed: {resp}"
        );

        // Shut down the upstream server
        let _ = shutdown_tx.send(());
        // Give the server time to close
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

        // The guest connection should close (recv returns empty or connection
        // closed). If the old bug were present, we'd get a 502 instead.
        let resp = conn.recv(1000).await;
        assert!(
            !resp.contains("502"),
            "should not get 502 after upstream close: {resp}"
        );
    });
}
