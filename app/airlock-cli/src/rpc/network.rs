//! Sandbox network service for the guest.
//!
//! Gives the guest access to the sandbox network on the host.

use std::os::unix::io::OwnedFd;

use airlock_common::network_capnp::network_proxy;
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp};

use super::{Driver, driver, vsock_transport};
use crate::network::Network;

/// Serve the sandbox network to the guest.
/// Args:
///  - `vsock_fd`: Connected vsock socket of the network channel
///  - `network`: Network that the guest uses. It lives exactly as long as
///    the returned driver
///
/// Returns:
///   [`Driver`] that serves the connection until it drops.
pub fn serve_network(vsock_fd: OwnedFd, network: Network) -> anyhow::Result<Driver> {
    // The network has its own vsock connection (`NETWORK_PORT`), so bulk
    // byte relays do not cause head-of-line blocking of the supervisor RPC.
    // `network` is the bootstrap capability.
    let transport = vsock_transport(vsock_fd, rpc_twoparty_capnp::Side::Server)?;
    let bootstrap: network_proxy::Client = capnp_rpc::new_client(network);
    let rpc = RpcSystem::new(transport, Some(bootstrap.client));
    Ok(driver(rpc, "network"))
}
