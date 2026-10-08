//! In-VM supervisor (`airlockd`).
//!
//! Runs as the first process (PID 1) in the guest Linux VM. Connects to the
//! host CLI and serves its requests. The host uses airlockd to set up the
//! guest environment (mounts, networking, DNS) and to run processes in the
//! sandbox.

mod admin;
mod bridge;
mod browser;
mod clipboard;
mod daemon;
mod init;
mod logging;
mod net;
mod process;
mod rpc;
mod sandbox_ns;
mod stats;
#[cfg(test)]
mod test_cfg;
mod util;
mod vsock;

use std::os::unix::io::{FromRawFd, IntoRawFd, OwnedFd};
use std::rc::Rc;

use airlock_common::network_capnp::network_proxy;
use capnp_rpc::{RpcSystem, rpc_twoparty_capnp, twoparty};
use futures::AsyncReadExt;
use tokio::task::LocalSet;
use tracing::info;

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::large_futures)]
async fn main() -> anyhow::Result<()> {
    let local = LocalSet::new();
    local.run_until(airlockd()).await?;
    Ok(())
}

/// Run the supervisor for the lifetime of the VM.
///
/// Accepts the host CLI connections (supervisor and network), sets up the
/// guest on `boot` and then waits until the host stops the VM. The `boot`
/// call starts no process. The host starts all processes (the main shell,
/// `airlock exec`) later with `spawn`.
async fn airlockd() -> anyhow::Result<()> {
    // PID 1 must reap orphaned zombies (for example from double-forking
    // daemons). Start the reaper before any process starts.
    tokio::task::spawn_local(process::run_orphan_reaper());

    // Accept the supervisor channel first. The accept blocks until the host
    // connects.
    let sup_listen = vsock::listen(airlock_common::SUPERVISOR_PORT)?;
    let sup_conn = vsock::accept(&sup_listen)?;
    drop(sup_listen);

    // Accept the network channel second. The host opens it right after the
    // supervisor channel. Bulk transfers on `NetworkProxy.connect` use
    // their own socket buffers, so they cannot block pty, stats or daemon
    // traffic on the supervisor channel (head-of-line blocking).
    let net_listen = vsock::listen(airlock_common::NETWORK_PORT)?;
    let net_conn = vsock::accept(&net_listen)?;
    drop(net_listen);
    let network = bootstrap_network_client(net_conn)?;

    let admin_state = admin::AdminState::new();
    let deny_tracker = admin_state.deny_tracker.clone();

    rpc::serve(sup_conn, deny_tracker, network, async |cfg| {
        logging::init(cfg.log_sink, &cfg.log_filter);

        info!("setup vm");
        init::setup(
            &cfg.init_config,
            &cfg.mount_config,
            &cfg.sockets,
            cfg.nested_virt,
        )?;
        // Give resources back to the host at intervals. Trim the sparse
        // disk image and drop the dentry/inode slab, so that the virtiofs
        // proxy on the host does not collect more and more FDs.
        init::start_periodic_maintenance();

        let dns = Rc::new(net::DnsState::new());
        net::start_dns(dns.clone()).await?;
        net::start_host_socket_forward(&cfg.network, cfg.sockets)?;
        net::start_host_port_forward(&cfg.init_config.host_ports, cfg.network.clone()).await?;
        net::start_tcp_proxy(cfg.network.clone(), dns)?;
        admin::start(admin_state.clone()).await?;
        clipboard::start(cfg.clipboard, cfg.uid, cfg.gid)?;
        browser::start(cfg.browser, cfg.uid, cfg.gid)?;

        if !cfg.daemons.is_empty() {
            info!("starting {} daemon(s)", cfg.daemons.len());
            let set = daemon::DaemonSet::start_all(cfg.daemons, cfg.uid, cfg.gid);
            *cfg.daemon_set_slot.borrow_mut() = Some(set);
        }

        info!("boot complete");

        Ok(())
    })
    .await?;

    // Keep the supervisor alive until the CLI stops the VM. PID 1 waits
    // here in both cases:
    //  * Boot succeeded: the host runs processes with `spawn`.
    //  * Boot failed: the host received the RPC error and decides if it
    //    stops the VM.
    std::future::pending::<()>().await;

    Ok(())
}

/// Make a `NetworkProxy` client from the accepted network channel.
/// Args:
///  - `conn_fd`: Accepted vsock connection of the network channel
///
/// Returns:
///   `NetworkProxy` client capability, or error if the socket setup fails.
fn bootstrap_network_client(conn_fd: OwnedFd) -> anyhow::Result<network_proxy::Client> {
    // The guest is the capnp *client* here, because the host serves the
    // bootstrap `NetworkProxy`. The guest accepted the vsock connection,
    // but vsock direction and capnp side are independent.
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(conn_fd.into_raw_fd()) };
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::TcpStream::from_std(std_stream)?;
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();
    let transport = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Client,
        capnp::message::ReaderOptions::default(),
    );
    let mut rpc = RpcSystem::new(Box::new(transport), None);
    let network: network_proxy::Client = rpc.bootstrap(rpc_twoparty_capnp::Side::Server);
    tokio::task::spawn_local(rpc);
    Ok(network)
}
