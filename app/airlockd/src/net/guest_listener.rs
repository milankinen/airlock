//! Guest listeners for port forwards from the guest to the host.
//!
//! Binds guest loopback ports and relays each accepted connection to the
//! host, with bytes in both directions. Two kinds of listeners exist:
//!  * Host-published ports from the config. They start at boot, before user
//!    processes and daemons, and live until the VM stops. Their connections
//!    go through the host network proxy, so the host rules apply.
//!  * Run-time forwards on request of the host. Their connections go to a
//!    host-side connector with no rules and no interception. The listener
//!    lives as long as the host keeps its lifetime token. When the host
//!    releases the token, the port becomes free and all its connections
//!    close.

use airlock_common::network_capnp::{network_proxy, tcp_sink};
use airlock_common::supervisor_capnp::{listen_handle, local_connector};
use bytes::Bytes;
use tokio::net::{TcpListener, TcpStream};
use tokio::task::{AbortHandle, JoinSet};
use tracing::{debug, error, info};

use super::rpc_bridge::{ChannelSink, relay, rpc_connect_tcp};

/// Bind a listener on `127.0.0.1:<port>` for each host-published port and
/// start an accept loop for each listener. The listeners live until the VM
/// stops.
///
/// Must run before user processes and daemons start. Then no user process
/// can bind one of the ports before the supervisor does.
/// Args:
///  - `ports`: Host-published ports
///  - `network`: Host network proxy client
///
/// Returns:
///   Error if a bind fails.
pub async fn start_host_port_forward(
    ports: &[u16],
    network: network_proxy::Client,
) -> anyhow::Result<()> {
    for &port in ports {
        // The loop runs until the VM stops, so the abort handle is not kept.
        listen(port, HostTarget::NetworkProxy(network.clone())).await?;
    }
    Ok(())
}

/// Bind `127.0.0.1:<port>` and start an accept loop that sends each
/// connection to `connector`.
/// Args:
///  - `port`: Guest loopback port
///  - `connector`: Host-side target of the accepted connections
///
/// Returns:
///   Lifetime token of the listener. The listener and its connections close
///   when the last reference to the token drops. Error if the bind fails.
pub async fn bind_host_port(
    port: u16,
    connector: local_connector::Client,
) -> anyhow::Result<listen_handle::Client> {
    // The loop holds no reference to the token. Thus a release of the token
    // by the host is the only thing that stops the loop.
    let task = listen(port, HostTarget::Connector(connector)).await?;
    Ok(capnp_rpc::new_client(ListenHandle { port, task }))
}

/// Host side of the accepted connections.
#[derive(Clone)]
enum HostTarget {
    /// Host network proxy, with the target `127.0.0.1:<port>`. The target is
    /// the same address, so the host-side handler can match it to its
    /// forwarding rule.
    NetworkProxy(network_proxy::Client),
    /// Host-side connector of a run-time forward.
    Connector(local_connector::Client),
}

impl HostTarget {
    /// Open the host side of one connection.
    /// Args:
    ///  - `port`: Guest loopback port of the listener
    ///  - `server_sink`: Sink that receives the bytes from the host
    ///
    /// Returns:
    ///   Host-side sink for the bytes from the guest.
    async fn open(
        &self,
        port: u16,
        server_sink: tcp_sink::Client,
    ) -> anyhow::Result<tcp_sink::Client> {
        match self {
            Self::NetworkProxy(network) => {
                rpc_connect_tcp(network, "127.0.0.1", port, server_sink).await
            }
            Self::Connector(connector) => {
                let mut req = connector.connect_request();
                req.get().set_client(server_sink);
                let reply = req.send().promise.await?;
                Ok(reply.get()?.get_server()?)
            }
        }
    }
}

/// Lifetime token of one run-time guest listener.
struct ListenHandle {
    port: u16,
    task: AbortHandle,
}

impl listen_handle::Server for ListenHandle {}

impl Drop for ListenHandle {
    /// Stop the accept loop. The loop owns the listener and the connection
    /// tasks, so the abort frees the port and closes all connections.
    fn drop(&mut self) {
        info!("guest listener down on 127.0.0.1:{}", self.port);
        self.task.abort();
    }
}

/// Bind `127.0.0.1:<port>` and start the accept loop in a task.
///
/// Returns:
///   Abort handle of the accept loop. An abort frees the port and closes
///   all connections. Error if the bind fails.
async fn listen(port: u16, target: HostTarget) -> anyhow::Result<AbortHandle> {
    let listener = TcpListener::bind(("127.0.0.1", port))
        .await
        .map_err(|e| anyhow::anyhow!("bind 127.0.0.1:{port}: {e}"))?;
    info!("guest listener up on 127.0.0.1:{port}");
    Ok(tokio::task::spawn_local(accept_loop(listener, port, target)).abort_handle())
}

/// Accept connections and relay each one in a task of its own.
async fn accept_loop(listener: TcpListener, port: u16, target: HostTarget) {
    // The set aborts the connection tasks when the loop drops. The number of
    // connections has no limit.
    let mut conns = JoinSet::new();
    loop {
        tokio::select! {
            res = listener.accept() => match res {
                Ok((stream, _)) => {
                    conns.spawn_local(handle(stream, port, target.clone()));
                }
                Err(e) => {
                    error!("guest listener accept 127.0.0.1:{port}: {e}");
                    // Prevent a busy loop on errors that persist, for
                    // example a full fd table.
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            },
            // Remove the finished connection tasks from the set.
            Some(_) = conns.join_next(), if !conns.is_empty() => {}
        }
    }
}

/// Relay one accepted guest connection to the host.
async fn handle(stream: TcpStream, port: u16, target: HostTarget) {
    debug!("guest listener connect 127.0.0.1:{port}");

    let (server_tx, mut server_rx) = tokio::sync::mpsc::channel::<Bytes>(1);
    let server_sink = capnp_rpc::new_client(ChannelSink::new(server_tx));

    // A failed open is expected (for example a deny by the host). The drop
    // of `stream` closes the guest socket.
    let client_sink = match target.open(port, server_sink).await {
        Ok(sink) => sink,
        Err(e) => {
            debug!("guest listener rpc 127.0.0.1:{port}: {e}");
            return;
        }
    };

    let (mut read, mut write) = stream.into_split();
    relay(&mut read, &mut write, client_sink, &mut server_rx).await;
    debug!("guest listener closed 127.0.0.1:{port}");
}
