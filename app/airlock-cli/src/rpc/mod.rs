//! Communication between the host and the VM.
//!
//! Lets the host control the supervisor that runs in the VM: boot the
//! sandbox, start processes and read their output. Also serves the host
//! services that the VM can use:
//!  * terminal input
//!  * log forwarding
//!  * the sandbox network
//!  * the host clipboard
//!  * opening pages in the user's browser
//!
//! The host can also open connections to ports inside the VM.

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

use airlock_common::cli_capnp::cli_service;
use airlock_common::supervisor_capnp::{self, pty_config};
use capnp_rpc::{rpc_twoparty_capnp, twoparty};
use futures::AsyncReadExt;
pub use network::serve_network;
pub use process::*;
pub use stdin::Stdin;
pub use supervisor::{BootRequest, DaemonSpec, DaemonState, MaskSpec, Supervisor};

/// A Cap'n Proto RPC system on one vsock connection. It serves the
/// connection until the peer disconnects. The caller owns it and decides
/// how long it runs: the sandbox session starts it as a transport task and
/// stops it only after the VM stops.
pub type Driver = Pin<Box<dyn Future<Output = ()>>>;

/// Wrap a connected vsock fd as a two-party Cap'n Proto transport.
/// Args:
///  - `vsock_fd`: Connected vsock socket
///  - `side`: RPC side of this end (client or server)
fn vsock_transport(
    vsock_fd: OwnedFd,
    side: rpc_twoparty_capnp::Side,
) -> anyhow::Result<Box<dyn capnp_rpc::VatNetwork<twoparty::VatId>>> {
    // On macOS, Virtualization.framework gives a socket that behaves like a
    // TCP stream. On Linux, the cloud-hypervisor vsock is a Unix stream.
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

/// Make a [`Driver`] from an RPC system. The driver logs how the system
/// ended.
fn driver(rpc: capnp_rpc::RpcSystem<twoparty::VatId>, name: &'static str) -> Driver {
    Box::pin(async move {
        if let Err(e) = rpc.await {
            tracing::debug!("{name} rpc: {e}");
        }
    })
}

/// Fill a `PtyConfig`. `Supervisor.spawn` and the `airlock exec` client
/// use it.
/// Args:
///  - `builder`: `PtyConfig` builder to fill
///  - `size`: Terminal size `(rows, cols)` for PTY mode, or `None` for
///    pipe mode
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

/// Fill a process start request of the sandbox CLI service. `airlock
/// exec` and `airlock acp` use it.
/// Args:
///  - `params`: Request params to fill
///  - `stdin`: Input of the process
///  - `pty_size`: Terminal size `(rows, cols)` for PTY mode, or `None` for
///    pipe mode
///  - `argv`: Command and its arguments. Must not be empty.
///  - `cwd`: Working directory in the guest
///  - `env`: Extra environment variables (`KEY=VALUE`)
pub fn fill_exec_request(
    mut params: cli_service::exec_params::Builder<'_>,
    stdin: supervisor_capnp::stdin::Client,
    pty_size: Option<(u16, u16)>,
    argv: &[String],
    cwd: &str,
    env: &[String],
) {
    params.set_stdin(stdin);
    set_pty(params.reborrow().init_pty(), pty_size);
    let (cmd, args) = argv.split_first().expect("argv holds the command");
    params.set_cmd(cmd);
    let mut args_b = params.reborrow().init_args(args.len() as u32);
    for (i, a) in args.iter().enumerate() {
        args_b.set(i as u32, a.as_str());
    }
    params.set_cwd(cwd);
    let mut env_b = params.init_env(env.len() as u32);
    for (i, e) in env.iter().enumerate() {
        env_b.set(i as u32, e.as_str());
    }
}

#[cfg(test)]
mod tests;
