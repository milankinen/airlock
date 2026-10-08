//! Tests for HTTP upgrades and CONNECT tunnels: after the switch, the
//! proxy relays raw bytes both ways, and refused switches close cleanly.

use std::sync::{Arc, Mutex};

use airlock_common::network_capnp::network_proxy;
use tokio::io::AsyncWriteExt;

use crate::test_cfg::network::*;
use crate::test_cfg::upstream::*;
use crate::test_cfg::*;

/// Start a plain-TCP upgrade-echo upstream and open a guest stream to it.
/// Returns the stream and the upstream port.
async fn upgrade_stream(proxy: &network_proxy::Client) -> (RpcStream, u16) {
    let port = serve_upgrade_echo().await.port();
    (TestConnection::local(proxy, port).await.into_stream(), port)
}

/// A config with one Lua middleware that runs `script`.
fn middleware(script: &'static str) -> TestNetworkConfig {
    TestNetworkConfig {
        middleware_scripts: vec![("mw", script)],
        ..Default::default()
    }
}

/// Test that a WebSocket upgrade switches to a raw byte relay in both
/// directions, with and without a middleware. Both cases use the same
/// relay. A middleware must not break the switch or lose the bytes that
/// follow the handshake.
///   1. Start a network without middleware, then one with a no-op
///      middleware
///   2. In each network, send a WebSocket handshake to an upgrade-echo
///      upstream
///   3. Check the 101 response and the raw bytes in both directions
#[test]
fn websocket_upgrade_relays_raw_bytes_with_and_without_middleware() {
    for config in [TestNetworkConfig::default(), middleware("-- noop")] {
        run_with_config(config, |proxy, _, _| async move {
            let (mut stream, port) = upgrade_stream(&proxy).await;
            assert_raw_relay(
                &mut stream,
                &websocket_handshake(port, true),
                "HTTP/1.1 101",
            )
            .await;
        });
    }
}

/// Test that a WebSocket upgrade works over the TLS MITM when the guest
/// offers no ALPN, also when the upstream prefers h2.
///   1. Start a TLS upgrade-echo upstream that offers h2 and HTTP/1.1
///   2. Connect the guest over TLS with no ALPN offer
///   3. Send a WebSocket handshake and check the 101 and raw relay
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

/// Test that a CONNECT request that the upstream accepts becomes a raw
/// byte tunnel.
///   1. Send a CONNECT request to an upgrade-echo upstream
///   2. Check the 200 response and the raw bytes in both directions
#[test]
fn connect_tunnel_relays_raw_bytes() {
    run_with_config(TestNetworkConfig::default(), |proxy, _, _| async move {
        let (mut stream, port) = upgrade_stream(&proxy).await;
        let request =
            format!("CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n");
        assert_raw_relay(&mut stream, &request, "HTTP/1.1 200").await;
    });
}

/// Test that the proxy closes the guest connection when the upstream
/// answers CONNECT with 204. A 204 is not a tunnel, so the proxy must not
/// keep the connection open as one.
///   1. Send a CONNECT request that asks the upstream for a 204
///   2. Read until the proxy closes the connection
///   3. Check the 204 response with `Connection: close`
#[test]
fn connect_answered_204_closes_guest_connection() {
    run_with_config(TestNetworkConfig::default(), |proxy, _, _| async move {
        let (mut stream, port) = upgrade_stream(&proxy).await;
        // The upgrade-echo server answers 204 and then keeps its side open.
        // Only the proxy can close the guest connection.
        let request = format!(
            "CONNECT 127.0.0.1:{port} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-Reply: 204\r\n\r\n"
        );
        stream.write_all(request.as_bytes()).await.unwrap();
        let resp = read_until_eof(&mut stream).await;
        assert!(resp.starts_with("HTTP/1.1 204"), "{resp}");
        assert!(resp.to_lowercase().contains("connection: close"), "{resp}");
    });
}

/// Test that the proxy closes the guest connection when the upstream
/// refuses a WebSocket upgrade.
///   1. Send a WebSocket handshake without `Sec-WebSocket-Key`
///   2. Read until the proxy closes the connection
///   3. Check the upstream 400 response, its body and `Connection: close`
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

/// Test that the network stack refuses an upgrade response that a
/// middleware forged. Without a real upstream switch, a raw relay has
/// nothing to connect to.
///   1. Add a middleware that sends the request and changes the status of
///      the upstream response to 101
///   2. Send a handshake that the upstream refuses with 400
///   3. Check that the 101 is refused and HTTP 502 is returned
///   4. Check that the body of the 502 comes from the upgrade check, not
///      from a script error
#[test]
fn websocket_upgrade_forged_by_middleware_is_refused() {
    let script = "local res = req:send()\nres.status = 101\n";
    run_with_config(middleware(script), |proxy, _, _| async move {
        let (mut stream, port) = upgrade_stream(&proxy).await;
        stream
            .write_all(websocket_handshake(port, false).as_bytes())
            .await
            .unwrap();
        let resp = read_until_eof(&mut stream).await;
        assert!(resp.starts_with("HTTP/1.1 502"), "{resp}");
        assert!(resp.to_lowercase().contains("connection: close"), "{resp}");
        assert!(
            resp.ends_with("upgrade not accepted by upstream\n"),
            "{resp}"
        );
    });
}
