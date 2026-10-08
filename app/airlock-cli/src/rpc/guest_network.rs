//! Host access to the guest loopback network.
//!
//! Lets the host open TCP connections to ports on the guest loopback. Reverse
//! port forwards and sign-in callbacks use this access.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_common::network_capnp::tcp_sink;
use airlock_common::supervisor_capnp::supervisor;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::network::io::{ChannelSink, RelayError, RpcTransport};

/// Guest loopback, for connections that the host opens. Wraps the
/// `openLocalTcp` call of the supervisor. Cheap to clone.
#[derive(Clone)]
pub struct GuestNetwork {
    supervisor: supervisor::Client,
}

impl GuestNetwork {
    /// Make a guest network handle from a supervisor client.
    pub fn new(supervisor: supervisor::Client) -> Self {
        Self { supervisor }
    }

    /// Open a TCP connection to `port` on the guest loopback.
    /// Returns:
    ///   Byte stream of the connection.
    pub async fn connect(&self, port: u16) -> anyhow::Result<RpcTransport> {
        let (tx, rx) = mpsc::channel::<Bytes>(1);
        let error: RelayError = Rc::new(RefCell::new(None));
        let client_sink: tcp_sink::Client = capnp_rpc::new_client(ChannelSink::new(tx, error));

        let mut req = self.supervisor.open_local_tcp_request();
        req.get().set_port(port);
        req.get().set_client(client_sink);
        let response = req.send().promise.await?;
        let server_sink = response.get()?.get_server()?;
        Ok(RpcTransport::new(Bytes::new(), rx, server_sink))
    }
}
