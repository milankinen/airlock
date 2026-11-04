//! Port forwards between the host and a running sandbox.
//!
//! Host programs connect to a host loopback port and reach a port on the
//! sandbox loopback, or sandbox programs connect to a sandbox loopback
//! port and reach a port on the host loopback. The sandbox process relays
//! the connections. The forwards do not hold the sandbox. They close when
//! the client stops them or the sandbox stops. A port conflict stops all
//! forwards.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr};
use std::rc::Rc;

use airlock_common::cli_capnp::cli_service;
use airlock_common::supervisor_capnp::{listen_handle, local_connector};
use tokio::net::TcpStream;
use tokio::task::JoinSet;

use super::client::Connection;
use crate::config::config_values::PortMapping;
use crate::messages;
use crate::network::{io, reverse_forward};
use crate::rpc::guest_network::GuestNetwork;

/// Direction of the forwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ForwardMode {
    /// Host programs connect to sandbox ports.
    Guest,
    /// Sandbox programs connect to host ports.
    Host,
}

/// Receiver of the state changes of the forwards.
pub trait ForwardReport {
    /// All forwards are active.
    fn ready(&self, mode: ForwardMode, forwards: &[PortMapping]);

    /// A connection to `guest_port` failed after the start, or after a
    /// connection that worked.
    fn connect_failed(&self, guest_port: u16, reason: &str);
}

/// Run the forwards until `stop` completes or the sandbox stops.
/// Args:
///  - `service`: CLI service of the running sandbox
///  - `connection`: The connection of `service`
///  - `mode`: Direction of the forwards
///  - `forwards`: Port pairs. The caller checks them with
///    `check_duplicates` before it connects.
///  - `stop`: Future that completes when the user stops the command
///  - `report`: Receives the state changes of the forwards
///
/// Returns:
///   `Ok` when `stop` completes. Error if a forward cannot start (then no
///   forward stays active), or if the sandbox stops.
pub async fn forward(
    service: cli_service::Client,
    mut connection: Connection,
    mode: ForwardMode,
    forwards: &[PortMapping],
    stop: impl Future<Output = ()>,
    report: Rc<dyn ForwardReport>,
) -> anyhow::Result<()> {
    // The connection ends when the sandbox stops. Then the sandbox
    // released the forwards.
    let mut stop = std::pin::pin!(stop);

    // The listener set aborts its tasks when it drops. Thus a return from
    // this function closes all host listeners.
    let mut listeners = JoinSet::new();
    let connections = Rc::new(RefCell::new(JoinSet::new()));
    let result = async {
        let setup = async {
            // The connection of a forward must not keep the sandbox.
            service.unref_request().send().promise.await?;
            match mode {
                ForwardMode::Guest => {
                    start_to_sandbox(&service, forwards, &mut listeners, &report).await?;
                    // The listener set holds the host listeners. The
                    // sandbox holds nothing, thus no handles.
                    Ok(Vec::new())
                }
                ForwardMode::Host => start_to_host(&service, forwards, &connections).await,
            }
        };
        // The guest releases the listeners when the handles drop.
        let _handles: Vec<listen_handle::Client> = tokio::select! {
            result = setup => result?,
            () = connection.closed() => anyhow::bail!(messages::network_forward::SANDBOX_STOPPED),
            () = &mut stop => return Ok(()),
        };
        report.ready(mode, forwards);

        tokio::select! {
            () = connection.closed() => anyhow::bail!(messages::network_forward::SANDBOX_STOPPED),
            () = &mut stop => Ok(()),
        }
    }
    .await;
    // The connectors that the sandbox holds share the connection set. Abort
    // the relays here, so that they do not wait for the release of the
    // connectors.
    connections.borrow_mut().abort_all();
    result
}

/// Bind the host ports of all forwards and start their accept loops in
/// `listeners`. Each connection goes to the sandbox through the CLI
/// service.
/// Returns:
///   Error on the first bind error. Then no port stays bound.
// The sandbox can refuse a connection, for example a loop with a config
// forward or no listener on the sandbox port. The command has no up-front
// check for this, because a sandbox program can start to listen later.
// Thus the forward stays active. A failure shows when the forward starts
// to fail: the first failure, and the first failure after a connection that
// worked. More failures in a row do not show.
async fn start_to_sandbox(
    service: &cli_service::Client,
    forwards: &[PortMapping],
    listeners: &mut JoinSet<()>,
    report: &Rc<dyn ForwardReport>,
) -> anyhow::Result<()> {
    let pairs = forwards.iter().map(|m| (m.host, m.guest)).collect();
    let bound = reverse_forward::bind(pairs).await?;
    let guest = GuestNetwork::from_cli(service.clone());
    for forward in bound {
        let port = forward.guest_port();
        let guest = guest.clone();
        let report = report.clone();
        let failing = Rc::new(Cell::new(false));
        reverse_forward::serve_with(forward, listeners, move |stream| {
            let guest = guest.clone();
            let report = report.clone();
            let failing = failing.clone();
            async move {
                let rpc_io = guest.connect(port).await.inspect_err(|e| {
                    if !failing.replace(true) {
                        report.connect_failed(port, &remote_reason(e));
                    }
                })?;
                failing.set(false);
                reverse_forward::relay_stream(stream, rpc_io).await;
                Ok(())
            }
        });
    }
    Ok(())
}

/// Ask the sandbox to listen on the sandbox port of each forward. Each
/// connection goes to the host port of the forward.
/// Args:
///  - `connections`: Task set for the relays of the connections
///
/// Returns:
///   Lifetime tokens of the sandbox listeners. Error on the first refused
///   listen. Then the tokens drop and no listener stays active.
async fn start_to_host(
    service: &cli_service::Client,
    forwards: &[PortMapping],
    connections: &Rc<RefCell<JoinSet<()>>>,
) -> anyhow::Result<Vec<listen_handle::Client>> {
    let mut handles = Vec::with_capacity(forwards.len());
    for m in forwards {
        let connector: local_connector::Client = capnp_rpc::new_client(HostConnector {
            host_port: m.host,
            connections: connections.clone(),
        });
        let mut req = service.bind_host_port_request();
        req.get().set_port(m.guest);
        req.get().set_connector(connector);
        let handle = req
            .send()
            .promise
            .await
            .and_then(|response| response.get()?.get_handle())
            .map_err(|e| {
                anyhow::anyhow!(messages::network_forward::listen_failed(
                    m.guest,
                    remote_reason(&e.into())
                ))
            })?;
        handles.push(handle);
    }
    Ok(handles)
}

/// Opens a host loopback connection for each connection that a sandbox
/// listener accepts.
struct HostConnector {
    host_port: u16,
    connections: Rc<RefCell<JoinSet<()>>>,
}

impl local_connector::Server for HostConnector {
    async fn connect(
        self: Rc<Self>,
        params: local_connector::ConnectParams,
        mut results: local_connector::ConnectResults,
    ) -> Result<(), capnp::Error> {
        let to_guest = params.get()?.get_client()?;
        // Try IPv4 first, the same as the config forwards. A host program
        // that listens only on `[::1]` also works.
        let addrs = [
            SocketAddr::from((Ipv4Addr::LOCALHOST, self.host_port)),
            SocketAddr::from((Ipv6Addr::LOCALHOST, self.host_port)),
        ];
        let stream = TcpStream::connect(&addrs[..]).await.map_err(|e| {
            capnp::Error::failed(format!("connect localhost:{}: {e}", self.host_port))
        })?;

        let (guest, from_guest) = io::rpc_pipe(to_guest);

        let mut connections = self.connections.borrow_mut();
        // Remove finished relays, so that the set does not grow.
        while connections.try_join_next().is_some() {}
        connections.spawn_local(reverse_forward::relay_stream(stream, guest));
        results.get().set_server(from_guest);
        Ok(())
    }
}

/// Get the error text that the sandbox gave, without the prefixes that
/// each RPC hop adds.
fn remote_reason(e: &anyhow::Error) -> String {
    let Some(e) = e.downcast_ref::<capnp::Error>() else {
        return format!("{e:#}");
    };
    let mut reason = e.extra.as_str();
    while let Some(rest) = reason.strip_prefix("remote exception: ") {
        reason = rest;
    }
    reason.to_string()
}
