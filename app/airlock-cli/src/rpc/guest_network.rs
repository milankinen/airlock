//! The guest's loopback network, reached from the host.
//!
//! [`GuestNetwork`] wraps the supervisor's `openLocalTcp`: the host opens a
//! TCP connection to a port on the guest's loopback and gets a byte
//! stream. Reverse port forwards ([`crate::network::reverse_forward`]) and
//! the sign-in callback forwards of the network services
//! ([`crate::services::callback`]) relay into the guest this way.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_common::network_capnp::tcp_sink;
use airlock_common::supervisor_capnp::supervisor;
use bytes::Bytes;
use tokio::sync::mpsc;

use crate::network::io::{ChannelSink, RelayError, RpcTransport};

/// The guest's loopback, for connections the host opens. Cheap to clone.
#[derive(Clone)]
pub struct GuestNetwork {
    supervisor: supervisor::Client,
}

impl GuestNetwork {
    pub fn new(supervisor: supervisor::Client) -> Self {
        Self { supervisor }
    }

    /// Open a TCP connection to `port` on the guest's loopback.
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
