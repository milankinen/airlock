use std::cell::Cell;
use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::Router;
use axum::routing::{get, post};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::Notify;

use super::helpers::*;
use crate::network::io::Transport;
use crate::network::tcp;

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
    run_network(vec!["example.com".into()], vec![], |proxy| async move {
        // Denies are deferred to the relay phase so that denied HTTP
        // requests can still be surfaced in the monitor. The connect
        // itself succeeds; the 403 comes back over the stream.
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", 80)
            .await
            .expect("connect is always accepted; deny surfaces at relay");
        let resp = conn.roundtrip(&http_get(80, "/")).await;
        assert!(resp.contains("403"), "expected 403, got: {resp}");
    });
}

#[test]
fn wildcard_host_allowed() {
    run_network(vec!["*.0.0.1".into()], vec![], |proxy| async move {
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
    run_network(vec![], vec![], |proxy| async move {
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", 80)
            .await
            .expect("connect is always accepted; deny surfaces at relay");
        let resp = conn.roundtrip(&http_get(80, "/")).await;
        assert!(resp.contains("403"), "expected 403, got: {resp}");
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

// ── Relay shutdown backpressure ─────────────────────────

/// Keeps offering one byte, then yields, forever — standing in for a guest
/// that keeps re-offering an upload as long as something keeps reading it.
/// Pings `drained` every time a byte is actually handed out.
struct AlwaysReady {
    drained: Arc<Notify>,
    ready: Cell<bool>,
}

impl AsyncRead for AlwaysReady {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.ready.replace(!self.ready.get()) {
            buf.put_slice(b"x");
            self.drained.notify_one();
            Poll::Ready(Ok(()))
        } else {
            // Yield every other poll so a caller that never stops reading
            // (like `relay`'s drain loop) can't spin forever on one future.
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    }
}

/// `shutdown` only completes once `drained` fires once — standing in for
/// `RpcTransport::poll_shutdown` waiting on a `close` ack that itself needs
/// the peer to keep draining.
struct GatedShutdown {
    drained: Arc<Notify>,
    waiting: Option<Pin<Box<dyn Future<Output = ()>>>>,
}

impl AsyncWrite for GatedShutdown {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if self.waiting.is_none() {
            let drained = self.drained.clone();
            self.waiting = Some(Box::pin(async move { drained.notified().await }));
        }
        match self.waiting.as_mut().unwrap().as_mut().poll(cx) {
            Poll::Ready(()) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

#[tokio::test]
async fn relay_drains_container_so_gated_shutdown_does_not_hang() {
    let drained = Arc::new(Notify::new());
    let container = Transport {
        read: Box::new(AlwaysReady {
            drained: drained.clone(),
            // Start not ready: if the main loop's c2s read handed out a byte
            // first, its stored `Notify` permit would open the gate without
            // the shutdown-phase drain ever running.
            ready: Cell::new(false),
        }),
        write: Box::new(GatedShutdown {
            drained,
            waiting: None,
        }),
        h2: false,
    };
    // The server side closes immediately, which is what ends the relay's
    // main loop and kicks off the shutdown this test is exercising.
    let server = Transport {
        read: Box::new(tokio::io::empty()),
        write: Box::new(tokio::io::sink()),
        h2: false,
    };

    // Without draining `container.read`, `GatedShutdown` never sees its
    // one required notification and `relay` would hang for the full
    // `RELAY_SHUTDOWN_TIMEOUT` (30s). A couple of seconds of headroom is
    // generous for CI while still failing fast if the drain regresses.
    let result = tokio::time::timeout(Duration::from_secs(2), tcp::relay(container, server)).await;
    assert!(
        result.is_ok(),
        "relay should finish once its drain unblocks the gated shutdown"
    );
}
