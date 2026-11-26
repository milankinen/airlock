//! Plain TCP support.
//!
//! Connects to the upstream server and relays raw bytes between the sandbox
//! and the server.

use std::net::SocketAddr;

use airlock_common::network_capnp::tcp_sink;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::debug;

use super::host::Host;
use super::io;
use super::target::ResolvedTarget;

/// Wrap the RPC channel to the guest in a [`io::Transport`]. This does not
/// connect to the real server. The allow path uses it with
/// [`connect_server`]. The deny path gives the transport to a hyper
/// instance that sends 403.
/// Args:
///  - `first`: Bytes that were already read from the guest
///  - `rx`: Receiver for more guest bytes
///  - `client_sink`: RPC sink for bytes to the guest.
pub fn container_transport(
    first: Bytes,
    rx: mpsc::Receiver<Bytes>,
    client_sink: tcp_sink::Client,
) -> io::Transport {
    let rpc_io = io::RpcTransport::new(first, rx, client_sink);
    let (cr, cw) = tokio::io::split(rpc_io);
    io::Transport {
        read: Box::new(cr),
        write: Box::new(cw),
        h2: false,
    }
}

/// Open a plain TCP connection to the real server, with a timeout.
/// Returns:
///   Server transport, or error if the connection fails or times out.
pub async fn connect_server(target: &ResolvedTarget) -> anyhow::Result<io::Transport> {
    let addr = format!("{}:{}", target.host, target.port);
    debug!("plain tcp: {addr}");
    let server = tokio::time::timeout(crate::constants::TCP_CONNECT_TIMEOUT, dial(target))
        .await
        .map_err(|_| anyhow::anyhow!("connection timed out: {addr}"))??;
    let (sr, sw) = server.into_split();
    Ok(io::Transport {
        read: Box::new(sr),
        write: Box::new(sw),
        h2: false,
    })
}

/// Check the addresses of an allowed target before the relay connects
/// to it. This is the second phase of the decision (see
/// [`crate::network::Network::resolve_target_in`]). It does something only
/// for a public-only target or a target with deny ranges. It resolves a
/// name one time and keeps the allowed addresses for [`dial`]. Thus a DNS
/// answer that changes between the check and the connect (DNS rebinding)
/// cannot get past it.
/// Returns:
///   Error if the lookup fails. A target with no allowed address is
///   denied ([`ResolvedTarget::deny`]).
pub async fn resolve(target: &mut ResolvedTarget) -> anyhow::Result<()> {
    if !target.allowed || target.allows_all_addresses() {
        return Ok(());
    }
    let addresses: Vec<SocketAddr> = match &target.host {
        Host::Ip(ip) => vec![SocketAddr::new(*ip, target.port)],
        Host::Name(name) => tokio::time::timeout(
            crate::constants::TCP_CONNECT_TIMEOUT,
            tokio::net::lookup_host((name.as_str(), target.port)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("lookup timed out: {name}"))??
        .collect(),
    };
    target.addresses = addresses
        .into_iter()
        .filter(|a| target.allows_address(a.ip()))
        .collect();
    if target.addresses.is_empty() {
        debug!(
            "denied: {}:{} has no allowed address",
            target.host, target.port
        );
        target.deny();
    }
    Ok(())
}

/// Open a TCP stream to `target`: to the addresses that [`resolve`] kept,
/// else to the host.
/// Returns:
///   The stream, or error if the connection fails. Also an error for a
///   target that needs the address check of [`resolve`] and has no
///   addresses.
pub async fn dial(target: &ResolvedTarget) -> anyhow::Result<TcpStream> {
    if !target.addresses.is_empty() {
        return Ok(TcpStream::connect(&target.addresses[..]).await?);
    }
    anyhow::ensure!(
        target.allows_all_addresses(),
        "blocked: {}:{} has no checked address",
        target.host,
        target.port
    );
    Ok(match &target.host {
        Host::Ip(ip) => TcpStream::connect(SocketAddr::new(*ip, target.port)).await?,
        Host::Name(name) => TcpStream::connect((name.as_str(), target.port)).await?,
    })
}

/// Relay bytes in both directions between two transports, until one
/// direction closes.
pub async fn relay(mut container: io::Transport, mut server: io::Transport) {
    let c2s = async {
        let mut buf = vec![0u8; airlock_common::RELAY_CHUNK_SIZE];
        loop {
            match container.read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if server.write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    let s2c = async {
        let mut buf = vec![0u8; airlock_common::RELAY_CHUNK_SIZE];
        loop {
            match server.read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if container.write.write_all(&buf[..n]).await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    // When one direction finishes, shut down both sides at the same time,
    // with a timeout. An `RpcTransport` shutdown can block on a `close` ack
    // behind backpressure. Thus also read both read halves to the end, to
    // release the block.
    tokio::select! {
        () = c2s => {}
        () = s2c => {}
    }
    let shutdown = async { tokio::join!(server.write.shutdown(), container.write.shutdown()) };
    tokio::pin!(shutdown);
    let mut container_buf = vec![0u8; airlock_common::RELAY_CHUNK_SIZE];
    let mut server_buf = vec![0u8; airlock_common::RELAY_CHUNK_SIZE];
    let drain = async {
        tokio::join!(
            drain_to_eof(&mut container.read, &mut container_buf),
            drain_to_eof(&mut server.read, &mut server_buf),
        )
    };
    let _ = tokio::time::timeout(crate::constants::RELAY_SHUTDOWN_TIMEOUT, async {
        tokio::select! {
            _ = &mut shutdown => {}
            // Both sides got EOF or an error before the shutdown finished.
            // Continue to wait for the shutdown.
            _ = drain => { let _ = shutdown.await; }
        }
    })
    .await;
}

/// Read `src` to EOF or error, and discard the bytes. This keeps the acks
/// of the peer sends going when nothing else reads the stream.
async fn drain_to_eof(src: &mut io::BoxRead, buf: &mut [u8]) {
    loop {
        match src.read(buf).await {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the dial of checked addresses and the shutdown of the raw
    //! TCP relay.

    use std::cell::Cell;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::sync::Notify;

    use super::*;

    /// A guest that never stops sending. Each read gets one byte and
    /// signals `drained`. Every second poll is pending, so other tasks can
    /// run.
    struct AlwaysReady {
        drained: Arc<Notify>,
        ready: Cell<bool>,
    }

    impl AsyncRead for AlwaysReady {
        fn poll_read(
            self: Pin<&mut Self>,
            cx: &mut Context<'_>,
            buf: &mut ReadBuf<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.ready.replace(!self.ready.get()) {
                buf.put_slice(b"x");
                self.drained.notify_one();
                Poll::Ready(Ok(()))
            } else {
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }
    }

    /// A write half whose shutdown completes only after `drained` fires.
    /// It acts as an RPC `close` that waits until the guest data is read.
    struct GatedShutdown {
        drained: Arc<Notify>,
        waiting: Option<Pin<Box<dyn Future<Output = ()>>>>,
    }

    impl AsyncWrite for GatedShutdown {
        fn poll_write(
            self: Pin<&mut Self>,
            _cx: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            Poll::Ready(Ok(()))
        }

        fn poll_shutdown(
            mut self: Pin<&mut Self>,
            cx: &mut Context<'_>,
        ) -> Poll<std::io::Result<()>> {
            if self.waiting.is_none() {
                let drained = self.drained.clone();
                self.waiting = Some(Box::pin(async move { drained.notified().await }));
            }
            self.waiting
                .as_mut()
                .unwrap()
                .as_mut()
                .poll(cx)
                .map(|()| Ok(()))
        }
    }

    /// Test that the relay shutdown reads the guest stream while it waits for
    /// the guest close. Without this, a close that waits on backpressure
    /// blocks the relay until the shutdown timeout.
    ///   1. Make a guest that never stops sending and whose close waits until
    ///      someone reads its data
    ///   2. Make a server that closes at once
    ///   3. Check that the relay finishes in 2 seconds
    #[tokio::test]
    async fn relay_shutdown_drains_guest_so_gated_close_completes() {
        let drained = Arc::new(Notify::new());
        let container = io::Transport {
            read: Box::new(AlwaysReady {
                drained: drained.clone(),
                ready: Cell::new(false),
            }),
            write: Box::new(GatedShutdown {
                drained,
                waiting: None,
            }),
            h2: false,
        };
        let server = io::Transport {
            read: Box::new(tokio::io::empty()),
            write: Box::new(tokio::io::sink()),
            h2: false,
        };
        // The relay shutdown timeout is 30 s, so a pass in 2 s means the
        // drain released the close.
        tokio::time::timeout(Duration::from_secs(2), relay(container, server))
            .await
            .expect("relay finishes");
    }

    /// Test that the dial uses the addresses that the check kept and does
    /// not resolve the name again. A second lookup can give an other
    /// answer (DNS rebinding).
    ///   1. Resolve a target with a deny range for a name that has no DNS
    ///      record
    ///   2. Give it the address of a local listener as the checked address
    ///   3. Check that the dial connects to the listener
    #[tokio::test]
    async fn dial_uses_checked_addresses_and_does_not_resolve_again() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let network = crate::test_cfg::network::network_from_toml(
            "[network]\npolicy = \"allow-by-default\"\n\
             [network.rules.r]\ndeny = [\"10.0.0.0/8\"]\n",
        )
        .unwrap()
        .network;
        let mut target = network.resolve_target("no-record.invalid", addr.port());
        assert!(target.allowed && !target.allows_all_addresses());
        target.addresses = vec![addr];
        dial(&target).await.expect("dial to the checked address");
    }
}
