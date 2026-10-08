//! Tests for the plain HTTP/1.1 relay: requests and responses cross the
//! proxy intact, with and without middleware.

use airlock_common::network_capnp::network_proxy;
use axum::Router;
use axum::extract::Path;
use axum::http::{Method, StatusCode};
use axum::routing::{any, get};

use crate::test_cfg::network::*;
use crate::test_cfg::*;

/// An upstream that answers with the method, path and body it got. It also
/// has a large-body route and a 404 route with a custom header.
fn reflect_app() -> Router {
    Router::new()
        .route("/big", get(|| async { "x".repeat(100_000) }))
        .route(
            "/missing",
            get(|| async { (StatusCode::NOT_FOUND, [("x-custom", "test-value")], "nope") }),
        )
        .route(
            "/{*path}",
            any(
                |method: Method, Path(path): Path<String>, body: String| async move {
                    format!("{method} {path} {body}")
                },
            ),
        )
}

/// Send a GET, a POST, a 404 request and a large-body request through
/// `proxy`, and check that each response arrives intact.
async fn assert_http1_relay(proxy: &network_proxy::Client) {
    let port = serve(reflect_app()).await.port();

    let resp = TestConnection::local(proxy, port)
        .await
        .roundtrip(&http_get(port, "/a/b"))
        .await;
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.ends_with("GET a/b "), "{resp}");

    let resp = TestConnection::local(proxy, port)
        .await
        .roundtrip(&http_post(port, "/echo", "payload"))
        .await;
    assert!(resp.ends_with("POST echo payload"), "{resp}");

    let resp = TestConnection::local(proxy, port)
        .await
        .roundtrip(&http_get(port, "/missing"))
        .await;
    assert!(resp.starts_with("HTTP/1.1 404"), "{resp}");
    assert!(resp.contains("x-custom: test-value"), "{resp}");

    let resp = TestConnection::local(proxy, port)
        .await
        .roundtrip(&http_get(port, "/big"))
        .await;
    let (_, body) = resp.split_once("\r\n\r\n").unwrap();
    assert_eq!(body.len(), 100_000);
}

/// A config with one Lua middleware that does nothing. Each request and
/// response then goes through the middleware chain.
fn noop_middleware() -> TestNetworkConfig {
    TestNetworkConfig {
        middleware_scripts: vec![("noop", "-- noop")],
        ..Default::default()
    }
}

/// Test that HTTP/1.1 requests and responses cross the proxy intact when
/// no middleware applies.
///   1. Start a network without middleware
///   2. Send a GET, a POST, a 404 request and a 100 KB response request
///   3. Check the status, headers and bodies that the guest gets
#[test]
fn http1_requests_without_middleware_arrive_intact() {
    run_with_config(TestNetworkConfig::default(), |proxy, _, _| async move {
        assert_http1_relay(&proxy).await;
    });
}

/// Test that HTTP/1.1 requests and responses cross the proxy intact when
/// a middleware applies. A middleware that does nothing must not change
/// the messages.
///   1. Start a network with a no-op middleware
///   2. Send a GET, a POST, a 404 request and a 100 KB response request
///   3. Check the status, headers and bodies that the guest gets
#[test]
fn http1_requests_through_middleware_arrive_intact() {
    run_with_config(noop_middleware(), |proxy, _, _| async move {
        assert_http1_relay(&proxy).await;
    });
}

/// Test that one keep-alive guest connection can send more than one
/// request through the middleware path.
///   1. Open one guest connection through a no-op middleware
///   2. Send two keep-alive GET requests one after the other
///   3. Check that each request gets its own correct response
#[test]
fn http1_keepalive_connection_serves_several_requests() {
    run_with_config(noop_middleware(), |proxy, _, _| async move {
        let port = serve(reflect_app()).await.port();
        let mut conn = TestConnection::local(&proxy, port).await;
        for path in ["first", "second"] {
            conn.send(http_get_keepalive(port, &format!("/{path}")).as_bytes())
                .await;
            let resp = conn.recv(500).await;
            assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
            assert!(resp.ends_with(&format!("GET {path} ")), "{resp}");
        }
    });
}

/// Test that the proxy does not send a 502 to the guest when the upstream
/// closes an idle keep-alive connection. A false 502 can break a guest
/// client that waits for its next request.
///   1. Send one keep-alive GET through a no-op middleware
///   2. Stop the upstream server
///   3. Check that the guest gets no 502 on the connection
#[test]
fn http1_upstream_close_closes_guest_connection_without_502() {
    run_with_config(noop_middleware(), |proxy, _, _| async move {
        let (addr, shutdown) =
            serve_with_shutdown(Router::new().route("/", get(|| async { "hello" }))).await;
        let mut conn = TestConnection::local(&proxy, addr.port()).await;
        conn.send(http_get_keepalive(addr.port(), "/").as_bytes())
            .await;
        let resp = conn.recv(500).await;
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");

        // The graceful shutdown closes the idle keep-alive connection.
        let _ = shutdown.send(());
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
        let resp = conn.recv(1000).await;
        assert!(!resp.contains("502"), "{resp}");
    });
}
