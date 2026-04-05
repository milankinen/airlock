use bytes::Bytes;
use ezpez_protocol::supervisor_capnp::tcp_sink;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::debug;

use super::io;

/// Establish plain TCP connection.
pub async fn establish(
    addr: &str,
    first: Bytes,
    rx: mpsc::Receiver<Bytes>,
    client_sink: tcp_sink::Client,
) -> anyhow::Result<(io::Transport, io::Transport)> {
    debug!("plain tcp: {addr}");
    let rpc_io = io::RpcTransport::new(first, rx, client_sink);
    let server = TcpStream::connect(addr).await?;
    let (sr, sw) = server.into_split();
    let (cr, cw) = tokio::io::split(rpc_io);
    Ok((
        io::Transport {
            read: Box::new(cr),
            write: Box::new(cw),
            h2: false,
        },
        io::Transport {
            read: Box::new(sr),
            write: Box::new(sw),
            h2: false,
        },
    ))
}

/// Bidirectional relay between two transports.
pub async fn relay(mut container: io::Transport, mut server: io::Transport) {
    let c2s = async {
        let mut buf = [0u8; 8192];
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
        let _ = server.write.shutdown().await;
    };

    let s2c = async {
        let mut buf = [0u8; 8192];
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
        let _ = container.write.shutdown().await;
    };

    tokio::join!(c2s, s2c);
}
