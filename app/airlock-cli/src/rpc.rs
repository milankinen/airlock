//! Host-side Cap'n Proto RPC types for communicating with the in-VM supervisor.

pub(crate) mod browser;
pub(crate) mod clipboard;
pub(crate) mod guest_network;
mod logging;
mod network;
mod process;
mod stdin;
mod supervisor;

use std::future::Future;
use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};
use std::pin::Pin;

use airlock_common::supervisor_capnp::pty_config;
use capnp_rpc::{rpc_twoparty_capnp, twoparty};
use futures::AsyncReadExt;
pub use network::serve_network;
pub use process::*;
pub use stdin::Stdin;
pub use supervisor::{BootRequest, DaemonSpec, DaemonState, MaskSpec, Supervisor};

/// A Cap'n Proto RPC system bound to one vsock connection. It serves the
/// connection until the peer goes away. The caller owns it and decides how
/// long it runs: the sandbox session spawns it as a transport task and stops
/// it only after the VM has stopped.
pub type Driver = Pin<Box<dyn Future<Output = ()>>>;

/// Wrap a connected vsock fd as a two-party Cap'n Proto transport.
///
/// On macOS, Virtualization.framework hands out a socket that behaves like a
/// TCP stream; on Linux, cloud-hypervisor's vsock is a Unix stream.
fn vsock_transport(
    vsock_fd: OwnedFd,
    side: rpc_twoparty_capnp::Side,
) -> anyhow::Result<Box<dyn capnp_rpc::VatNetwork<twoparty::VatId>>> {
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
    Ok(Box::new(twoparty::VatNetwork::new(
        reader,
        writer,
        side,
        capnp::message::ReaderOptions::default(),
    )))
}

/// Turn a running RPC system into a [`Driver`] that logs how it ended.
fn driver(rpc: capnp_rpc::RpcSystem<twoparty::VatId>, name: &'static str) -> Driver {
    Box::pin(async move {
        if let Err(e) = rpc.await {
            tracing::debug!("{name} rpc: {e}");
        }
    })
}

/// Fill a `PtyConfig`: a terminal size selects PTY mode, `None` selects
/// pipe mode. Shared by `Supervisor.spawn` and the `airlock exec` client.
pub fn set_pty(mut builder: pty_config::Builder<'_>, size: Option<(u16, u16)>) {
    match size {
        Some((rows, cols)) => {
            let mut s = builder.init_size();
            s.set_rows(rows);
            s.set_cols(cols);
        }
        None => builder.set_none(()),
    }
}

#[cfg(test)]
mod tests;
