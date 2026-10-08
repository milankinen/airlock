use std::sync::{Arc, Mutex};

use airlock_common::network_capnp::network_proxy;
use tokio::io::AsyncWriteExt;

use crate::test_cfg::network::*;
use crate::test_cfg::upstream::*;
use crate::test_cfg::*;

async fn upgrade_stream(proxy: &network_proxy::Client) -> (RpcStream, u16) {
    let port = serve_upgrade_echo().await.port();
    (TestConnection::local(proxy, port).await.into_stream(), port)
}

fn middleware(script: &'static str) -> TestNetworkConfig {
    TestNetworkConfig {
        middleware_scripts: vec![("mw", script)],
        ..Default::default()
    }
}

#[test]
fn websocket_upgrade_without_middleware_relays_raw_bytes() {
    run_with_config(TestNetworkConfig::default(), |proxy, _, _| async move {
        let (mut stream, port) = upgrade_stream(&proxy).await;
        assert_raw_relay(
            &mut stream,
            &websocket_handshake(port, true),
            "HTTP/1.1 101",
        )
        .await;
    });
}

#[test]
fn websocket_upgrade_through_middleware_relays_raw_bytes() {
    run_with_config(middleware("-- noop"), |proxy, _, _| async move {
        let (mut stream, port) = upgrade_stream(&proxy).await;
        assert_raw_relay(
            &mut stream,
            &websocket_handshake(port, true),
            "HTTP/1.1 101",
        )
        .await;
    });
}

#[test]
fn websocket_upgrade_over_tls_without_alpn_relays_raw_bytes() {
    let upstream = FakeUpstream::bind(&[b"h2", b"http/1.1"]);
    run_with_config(
        TestNetworkConfig {
            trust_cas: vec![upstream.ca_pem()],
            ..Default::default()
        },
        |proxy, _, mitm_ca| async move {
            let port = upstream.port();
            upstream.serve_upgrade_echo(Arc::new(Mutex::new(Vec::new())));
            let mut tls = guest_tls(&proxy, &mitm_ca, port, &[]).await;
            assert_raw_relay(&mut tls, &websocket_handshake(port, true), "HTTP/1.1 101").await;
        },
    );
}

#[test]
fn connect_tunnel_relays_raw_bytes() {
    run_with_config(TestNetworkConfig::default(), |proxy, _, _| async move {
        let (mut stream, port) = upgrade_stream(&proxy).await;
        let request =
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        assert_raw_relay(&mut stream, &request, "HTTP/1.1 200").await;
    });
}

#[test]
fn connect_answered_204_closes_guest_connection() {
    run_with_config(TestNetworkConfig::default(), |proxy, _, _| async move {
        let (mut stream, port) = upgrade_stream(&proxy).await;
        let request = format!(
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Reply: 204\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let resp = read_until_eof(&mut stream).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert!(resp.to_lowercase().contains("connection: close"), "{resp}");
    });
}

#[test]
fn websocket_upgrade_rejected_by_upstream_closes_guest_connection() {
    run_with_config(TestNetworkConfig::default(), |proxy, _, _| async move {
        let (mut stream, port) = upgrade_stream(&proxy).await;
        stream
            .write_all(websocket_handshake(port, false).as_bytes())
            .await
            .unwrap();
        let resp = read_until_eof(&mut stream).await;
        assert!(resp.starts_with("HTTP/1.1 400"), "{resp}");
        assert!(resp.to_lowercase().contains("connection: close"), "{resp}");
        assert!(resp.ends_with("not-upgrade"), "{resp}");
    });
}

#[test]
fn websocket_upgrade_forged_by_middleware_is_refused() {
    run_with_config(middleware("res.status = 101"), |proxy, _, _| async move {
        let (mut stream, port) = upgrade_stream(&proxy).await;
        stream
            .write_all(websocket_handshake(port, false).as_bytes())
            .await
            .unwrap();
        let resp = read_until_eof(&mut stream).await;
        assert!(resp.starts_with("HTTP/1.1 502"), "{resp}");
        assert!(resp.to_lowercase().contains("connection: close"), "{resp}");
    });
}
