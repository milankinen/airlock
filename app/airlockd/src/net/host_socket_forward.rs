//! Unix socket forwarding from guest to host.
//!
//! Makes host Unix sockets, for example a Docker socket or an SSH agent,
//! available in the container. Each connection goes to the host socket through
//! the host network proxy.

use std::path::Path;

use airlock_common::network_capnp::{connect_result, network_proxy, tcp_sink};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tracing::{debug, error, info};

use super::rpc_bridge::ChannelSink;
use crate::rpc::SocketForwardConfig;

/// Bind a Unix listener for each socket pair, then start the accept loops.
///
/// The binds complete before the function returns. Thus all socket files
/// exist in the container rootfs before the container process starts.
/// Args:
///  - `network`: Host network proxy client
///  - `sockets`: Socket pairs to forward
///
/// Returns:
///   Error if a bind fails.
pub fn start(
    network: &network_proxy::Client,
    sockets: Vec<SocketForwardConfig>,
) -> anyhow::Result<()> {
    for sock in sockets {
        let listener = bind(&sock.guest)?;
        info!("socket forward: {} → {}", sock.guest, sock.host);
        let network = network.clone();
        tokio::task::spawn_local(async move {
            if let Err(e) = accept_loop(listener, &sock.host, &sock.guest, network).await {
                error!("socket forward {} → {}: {e}", sock.host, sock.guest);
            }
        });
    }
    Ok(())
}

/// Bind a `UnixListener` at the guest path inside the container rootfs, with
/// mode 0777.
///
/// The socket file is in `/mnt/overlay/rootfs`, so it goes to the overlayfs
/// upper layer and the container sees it.
fn bind(guest_path: &str) -> anyhow::Result<UnixListener> {
    // Resolve absolute symlink targets relative to the container root, as
    // in a chroot. Otherwise an absolute symlink such as `/var/run -> /run`
    // would send the bind to the VM's `/run/`, not to the container's.
    let root = Path::new("/mnt/overlay/rootfs");
    let full_path = crate::util::resolve_in_root(root, guest_path);
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove the old socket from a previous run
    let _ = std::fs::remove_file(&full_path);
    let listener = UnixListener::bind(&full_path)
        .map_err(|e| anyhow::anyhow!("bind {}: {e}", full_path.display()))?;
    std::fs::set_permissions(
        &full_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o777),
    )?;
    Ok(listener)
}

/// Accept container connections and relay each one in its own task.
async fn accept_loop(
    listener: UnixListener,
    _host_path: &str,
    guest_path: &str,
    network: network_proxy::Client,
) -> anyhow::Result<()> {
    loop {
        let (stream, _) = listener.accept().await?;
        let network = network.clone();
        let guest_path = guest_path.to_string();
        tokio::task::spawn_local(async move {
            if let Err(e) = relay(stream, &guest_path, &network).await {
                debug!("socket relay {guest_path}: {e}");
            }
        });
    }
}

/// Relay one container connection to the host socket of `guest_path`.
async fn relay(
    stream: tokio::net::UnixStream,
    guest_path: &str,
    network: &network_proxy::Client,
) -> anyhow::Result<()> {
    let (mut local_read, mut local_write) = stream.into_split();

    // RPC channel for data from the host to the local peer
    let (server_tx, mut server_rx) = tokio::sync::mpsc::channel::<Bytes>(1);
    let server_sink: tcp_sink::Client = capnp_rpc::new_client(ChannelSink::new(server_tx));

    // Call the host NetworkProxy.connect with the guest socket path. The CLI
    // maps the guest path to the host path (with tilde expansion).
    let mut req = network.connect_request();
    req.get().init_target().set_socket(guest_path);
    req.get().set_client(server_sink);

    let response = req.send().promise.await?;
    let result = response.get()?.get_result()?;
    let client_sink = match result.which() {
        Ok(connect_result::Server(Ok(sink))) => sink,
        Ok(connect_result::Denied(Ok(reason))) => {
            let reason = reason.to_str().unwrap_or("unknown");
            anyhow::bail!("socket denied: {reason}");
        }
        _ => anyhow::bail!("invalid connect result"),
    };

    // Relay in both directions (local and RPC), with half-close. Each
    // direction runs to its end independently, so an EOF in one direction
    // closes only that direction. A `select!` that closed both on the first
    // EOF cut each "request, half-close, wait for reply" protocol.
    let local_to_rpc = async {
        let mut buf = vec![0u8; airlock_common::RELAY_CHUNK_SIZE];
        loop {
            match local_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut req = client_sink.send_request();
                    req.get().set_data(&buf[..n]);
                    if req.send().await.is_err() {
                        break;
                    }
                }
            }
        }
        // The local peer sends no more. Send EOF to the remote, and continue
        // to read from it.
        let _ = client_sink.close_request().send().promise.await;
    };

    let rpc_to_local = async {
        while let Some(data) = server_rx.recv().await {
            if local_write.write_all(&data).await.is_err() {
                break;
            }
        }
        // The remote sends no more. Send EOF to the local peer.
        let _ = local_write.shutdown().await;
    };

    // Run both directions to their end, so a one-way close is a half-close.
    tokio::join!(local_to_rpc, rpc_to_local);
    Ok(())
}
