//! Host access to the guest loopback network.
//!
//! Lets the host open TCP connections to ports on the guest loopback. Reverse
//! port forwards and sign-in callbacks use this access. The access goes
//! directly to the supervisor, or through the `airlock start` process of a
//! running sandbox.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_common::cli_capnp::cli_service;
use airlock_common::network_capnp::tcp_sink;
use airlock_common::supervisor_capnp::supervisor;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::network::io::{ChannelSink, RelayError, RpcTransport};

/// Guest loopback, for connections that the host opens. Wraps the
/// `connectToGuestPort` call of the supervisor or of the CLI service. Cheap
/// to clone.
#[derive(Clone)]
pub struct GuestNetwork {
    opener: Opener,
}

/// RPC client that has the `connectToGuestPort` call.
#[derive(Clone)]
enum Opener {
    Supervisor(supervisor::Client),
    Cli(cli_service::Client),
}

impl GuestNetwork {
    /// Make a guest network handle from a supervisor client.
    pub fn new(supervisor: supervisor::Client) -> Self {
        Self {
            opener: Opener::Supervisor(supervisor),
        }
    }

    /// Make a guest network handle from the CLI service client of a running
    /// sandbox.
    pub fn from_cli(service: cli_service::Client) -> Self {
        Self {
            opener: Opener::Cli(service),
        }
    }

    /// Open a TCP connection to `port` on the guest loopback.
    /// Returns:
    ///   Byte stream of the connection.
    pub async fn connect(&self, port: u16) -> anyhow::Result<RpcTransport> {
        let (tx, rx) = mpsc::channel::<Bytes>(1);
        let error: RelayError = Rc::new(RefCell::new(None));
        let client_sink: tcp_sink::Client = capnp_rpc::new_client(ChannelSink::new(tx, error));

        // The two calls have the same parameters and results, but different
        // generated types.
        let server_sink = match &self.opener {
            Opener::Supervisor(client) => {
                let mut req = client.connect_to_guest_port_request();
                req.get().set_port(port);
                req.get().set_client(client_sink);
                req.send().promise.await?.get()?.get_server()?
            }
            Opener::Cli(client) => {
                let mut req = client.connect_to_guest_port_request();
                req.get().set_port(port);
                req.get().set_client(client_sink);
                req.send().promise.await?.get()?.get_server()?
            }
        };
        Ok(RpcTransport::new(Bytes::new(), rx, server_sink))
    }
}
