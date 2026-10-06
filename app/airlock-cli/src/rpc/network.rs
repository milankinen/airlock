//! Host-side server for the `NetworkProxy` RPC channel.
//!
//! Runs on a dedicated vsock connection (`NETWORK_PORT`) so bulk byte
//! relays don't head-of-line-block the supervisor RPC. The bootstrap
//! capability is the [`Network`](crate::network::Network) impl of
//! `network_proxy::Server`.

use std::os::unix::io::OwnedFd;

use airlock_common::network_capnp::network_proxy;
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp};

use super::{Driver, driver, vsock_transport};
use crate::network::Network;

/// Consume `network` and serve it as the bootstrap capability of a
/// Cap'n Proto RPC system bound to the given vsock fd. The returned
/// [`Driver`] serves the connection until it drops; `Network` lives
/// exactly as long as the driver.
pub fn serve_network(vsock_fd: OwnedFd, network: Network) -> anyhow::Result<Driver> {
    let transport = vsock_transport(vsock_fd, rpc_twoparty_capnp::Side::Server)?;
    let bootstrap: network_proxy::Client = capnp_rpc::new_client(network);
    let rpc = RpcSystem::new(transport, Some(bootstrap.client));
    Ok(driver(rpc, "network"))
}
