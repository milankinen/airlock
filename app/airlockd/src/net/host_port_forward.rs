//! Per-port loopback listeners for host-published ports.
//!
//! For each port the host requested the guest expose, airlockd owns a
//! `TcpListener` on `127.0.0.1:<port>`. Connections from guest processes
//! are accepted and bridged to the host via `NetworkProxy.connect`, with
//! target set to that same `127.0.0.1:<port>` so the host-side handler
//! can match it to its forwarding rule.
//!
//! Listeners are bound during setup, before any guest process or daemon
//! starts, so there is no race where a user process binds the port first.
//!
//! Outbound traffic to destinations *other* than these loopback ports
//! flows through the TCP proxy on the TUN (see `net::tcp_proxy`).

use airlock_common::network_capnp::network_proxy;
use bytes::Bytes;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use super::rpc_bridge::{ChannelSink, relay, rpc_connect_tcp};

/// Bind a loopback listener per port and spawn an accept loop for each.
/// Must run before user processes / daemons start so the supervisor
/// wins the bind race for every port in the list.
pub async fn start(ports: &[u16], network: network_proxy::Client) -> anyhow::Result<()> {
    for &port in ports {
        let listener = TcpListener::bind(("127.0.0.1", port))
            .await
            .map_err(|e| anyhow::anyhow!("bind 127.0.0.1:{port}: {e}"))?;
        info!("host-port listener up on 127.0.0.1:{port}");

        let network = network.clone();
        tokio::task::spawn_local(async move {
            loop {
                let (stream, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(e) => {
                        error!("host-port accept 127.0.0.1:{port}: {e}");
                        continue;
                    }
                };
                let network = network.clone();
                tokio::task::spawn_local(async move {
                    handle(stream, port, &network).await;
                });
            }
        });
    }
    Ok(())
}

/// Accept the guest connection, open an RPC connection to the host for
/// `127.0.0.1:<port>`, and relay bytes both ways.
async fn handle(stream: tokio::net::TcpStream, port: u16, network: &network_proxy::Client) {
    debug!("host-port connect 127.0.0.1:{port}");

    let (server_tx, mut server_rx) = tokio::sync::mpsc::channel::<Bytes>(1);
    let server_sink = capnp_rpc::new_client(ChannelSink::new(server_tx));

    let client_sink = match rpc_connect_tcp(network, "127.0.0.1", port, server_sink).await {
        Ok(sink) => sink,
        Err(e) => {
            debug!("host-port rpc 127.0.0.1:{port}: {e}");
            return;
        }
    };

    let (mut read, mut write) = stream.into_split();
    relay(&mut read, &mut write, client_sink, &mut server_rx).await;
    debug!("host-port closed 127.0.0.1:{port}");
}
