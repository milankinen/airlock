use airlock_common::network_capnp::tcp_sink;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::debug;

use super::io;

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
pub async fn connect_server(addr: &str) -> anyhow::Result<io::Transport> {
    debug!("plain tcp: {addr}");
    let server = tokio::time::timeout(
        crate::constants::TCP_CONNECT_TIMEOUT,
        TcpStream::connect(addr),
    )
    .await
    .map_err(|_| anyhow::anyhow!("connection timed out: {addr}"))??;
    let (sr, sw) = server.into_split();
    Ok(io::Transport {
        read: Box::new(sr),
        write: Box::new(sw),
        h2: false,
    })
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
