//! Host-published ports in the guest.
//!
//! Makes each host-published port available to guest processes on
//! `127.0.0.1:<port>`. The connections go to the host port through the host
//! network proxy. Traffic to all other destinations goes through the outgoing
//! TCP proxy.

use airlock_common::network_capnp::network_proxy;
use bytes::Bytes;
use tokio::net::TcpListener;
use tracing::{debug, error, info};

use super::rpc_bridge::{ChannelSink, relay, rpc_connect_tcp};

/// Bind a listener on `127.0.0.1:<port>` for each port and start an accept
/// loop for each listener.
///
/// Must run before user processes and daemons start. Then no user process
/// can bind one of the ports before the supervisor does.
/// Args:
///  - `ports`: Host-published ports
///  - `network`: Host network proxy client
///
/// Returns:
///   Error if a bind fails.
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

/// Relay one accepted guest connection to the host.
///
/// Opens an RPC connection to the host with the target `127.0.0.1:<port>`,
/// then relays bytes in both directions. The target is the same address,
/// so the host-side handler can match it to its forwarding rule.
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
