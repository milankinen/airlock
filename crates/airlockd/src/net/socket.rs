//! Unix socket forwarding from guest to host.
//!
//! For each configured socket pair, a Unix listener is created inside the VM.
//! When a guest process connects, the connection is relayed to the host via
//! the `NetworkProxy` RPC interface, which connects to the corresponding
//! host-side Unix socket (e.g. a Docker socket or SSH agent).

use std::cell::RefCell;
use std::path::Path;

use airlock_protocol::supervisor_capnp::{connect_result, network_proxy, tcp_sink};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixListener;
use tracing::{debug, error, info};

use super::proxy::ChannelSink;
use crate::rpc::SocketForwardConfig;

/// Bind all socket listeners synchronously, then spawn the accept loops.
///
/// Binding is done before returning so all socket files exist in the
/// container rootfs before the container process starts.
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

/// Bind a UnixListener at the guest path inside the container rootfs.
///
/// Creates the socket file at the resolved path within `/mnt/overlay/rootfs`
/// so it lands in the overlayfs upper layer and is visible inside the container.
///
/// Path resolution treats absolute symlink targets as relative to the container
/// root, mirroring chroot semantics. Without this, an absolute symlink like
/// `/var/run -> /run` would redirect the bind to the VM's `/run/`, not the
/// container's `/run/`.
fn bind(guest_path: &str) -> anyhow::Result<UnixListener> {
    let root = Path::new("/mnt/overlay/rootfs");
    let full_path = crate::util::resolve_in_root(root, guest_path);
    if let Some(parent) = full_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Remove stale socket from previous run
    let _ = std::fs::remove_file(&full_path);
    let listener = UnixListener::bind(&full_path)
        .map_err(|e| anyhow::anyhow!("bind {}: {e}", full_path.display()))?;
    std::fs::set_permissions(
        &full_path,
        std::os::unix::fs::PermissionsExt::from_mode(0o777),
    )?;
    Ok(listener)
}

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

async fn relay(
    stream: tokio::net::UnixStream,
    guest_path: &str,
    network: &network_proxy::Client,
) -> anyhow::Result<()> {
    let (mut local_read, mut local_write) = stream.into_split();

    // Set up RPC channel for server→local data
    let (server_tx, mut server_rx) = tokio::sync::mpsc::channel::<Bytes>(1);
    let server_sink: tcp_sink::Client =
        capnp_rpc::new_client(ChannelSink(RefCell::new(Some(server_tx))));

    // Call host NetworkProxy.connect with the guest socket path.
    // The CLI maps guest → host path (with tilde expansion) on its side.
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

    // Bidirectional relay: local ↔ RPC.
    // When either direction closes, clean up both sides.
    let local_to_rpc = async {
        let mut buf = [0u8; 8192];
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
    };

    let rpc_to_local = async {
        while let Some(data) = server_rx.recv().await {
            if local_write.write_all(&data).await.is_err() {
                break;
            }
        }
    };

    tokio::select! {
        () = local_to_rpc => {}
        () = rpc_to_local => {}
    }

    // Clean up both sides regardless of which direction closed
    let _ = client_sink.close_request().send().promise.await;
    let _ = local_write.shutdown().await;
    Ok(())
}
