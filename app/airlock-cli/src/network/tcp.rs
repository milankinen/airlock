use airlock_common::network_capnp::tcp_sink;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::debug;

use super::io;
use super::target::{self, ResolvedTarget};

/// Wrap the RPC channel to the guest in a `Transport` without touching the
/// real server. Used on both the allow path (paired with `connect_server`)
/// and the deny path (where we hand the transport to a 403-serving hyper
/// instance instead).
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

/// Open a plain TCP socket to the real server.
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

/// Open a TCP stream to `target`. A [`ResolvedTarget::public_only`]
/// target resolves here, and only its public addresses are dialed: the
/// check covers the addresses the stream connects to, so neither a name
/// like `localhost` nor DNS rebinding gets past it.
pub async fn dial(target: &ResolvedTarget) -> anyhow::Result<TcpStream> {
    let addr = format!("{}:{}", target.host, target.port);
    if !target.public_only {
        return Ok(TcpStream::connect(&addr).await?);
    }
    let host = target::ip_literal(&target.host).map_or(target.host.clone(), |ip| ip.to_string());
    let public: Vec<_> = tokio::net::lookup_host((host.as_str(), target.port))
        .await?
        .filter(|a| target::is_public_ip(a.ip()))
        .collect();
    anyhow::ensure!(!public.is_empty(), "blocked: {addr} has no public address");
    Ok(TcpStream::connect(&public[..]).await?)
}

/// Bidirectional relay between two transports.
///
/// When either direction closes, both sides shut down concurrently under a
/// timeout. An `RpcTransport` shutdown can block on a `close` ack behind
/// backpressure, so shutdown also drains both read halves to unblock it.
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

    // When either direction finishes, shut down everything
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
            // Both sides hit EOF/error before the shutdown did; keep
            // waiting for it instead of abandoning it.
            _ = drain => { let _ = shutdown.await; }
        }
    })
    .await;
}

/// Reads `src` to EOF or error, discarding the bytes — keeps a peer's own
/// sends acking while nothing else is draining it.
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
    use std::cell::Cell;
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use std::time::Duration;

    use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
    use tokio::sync::Notify;

    use super::*;

    /// A guest that keeps offering one more byte as long as it is read,
    /// pinging `drained` for each byte taken.
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

    /// A write half whose shutdown completes only after `drained` fires,
    /// like an RPC `close` that waits for the peer to keep draining.
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
        tokio::time::timeout(Duration::from_secs(2), relay(container, server))
            .await
            .expect("relay finishes");
    }
}
