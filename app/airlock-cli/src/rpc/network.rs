//! Host-side server for the `NetworkProxy` RPC channel.
//!
//! Runs on a dedicated vsock connection (`NETWORK_PORT`) so bulk byte
//! relays don't head-of-line-block the supervisor RPC. The bootstrap
//! capability is the [`Network`](crate::network::Network) impl of
//! `network_proxy::Server`.

use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};

use airlock_common::network_capnp::network_proxy;
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use futures::AsyncReadExt;

use crate::network::Network;

/// Consume `network` and serve it as the bootstrap capability of a
/// Cap'n Proto RPC system bound to the given vsock fd. The RpcSystem
/// runs on the current `LocalSet` until the connection drops.
pub fn serve_network(vsock_fd: OwnedFd, network: Network) -> anyhow::Result<()> {
    #[cfg(target_os = "macos")]
    let stream = {
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(vsock_fd.into_raw_fd()) };
        std_stream.set_nonblocking(true)?;
        tokio::net::TcpStream::from_std(std_stream)?
    };
    #[cfg(target_os = "linux")]
    let stream = {
        let std_stream =
            unsafe { std::os::unix::net::UnixStream::from_raw_fd(vsock_fd.into_raw_fd()) };
        std_stream.set_nonblocking(true)?;
        tokio::net::UnixStream::from_std(std_stream)?
    };
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

    let transport = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        capnp::message::ReaderOptions::default(),
    );
    let bootstrap: network_proxy::Client = capnp_rpc::new_client(network);
    let rpc = RpcSystem::new(Box::new(transport), Some(bootstrap.client));
    tokio::task::spawn_local(rpc);
    Ok(())
}
