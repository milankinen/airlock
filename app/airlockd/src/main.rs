//! In-VM supervisor process.
//!
//! Runs as PID 1 inside the guest Linux VM. Accepts two vsock
//! connections from the host CLI — the supervisor RPC channel and the
//! network-proxy RPC channel — bootstraps the guest environment
//! (mounts, networking, DNS), and spawns the user's command.

mod admin;
mod daemon;
mod init;
mod logging;
mod net;
mod process;
mod rpc;
mod stats;
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

/// Single-connection lifecycle: accept the host CLI connections
/// (supervisor + network), set up the guest, run the user's process,
/// then idle until the VM is torn down.
async fn airlockd() -> anyhow::Result<()> {
    // Supervisor channel first — accept blocks until the host connects.
    let sup_listen = vsock::listen(airlock_common::SUPERVISOR_PORT)?;
    let sup_conn = vsock::accept(&sup_listen)?;
    drop(sup_listen);

    // Network channel second. The host opens this right after the
    // supervisor one so bulk transfers on `NetworkProxy.connect` get
    // their own socket buffers and cannot head-of-line-block pty /
    // stats / daemon traffic on the supervisor channel.
    let net_listen = vsock::listen(airlock_common::NETWORK_PORT)?;
    let net_conn = vsock::accept(&net_listen)?;
    drop(net_listen);
    let network = bootstrap_network_client(net_conn)?;

    let admin_state = admin::AdminState::new();
    let deny_tracker = admin_state.deny_tracker.clone();

    let exit_code = rpc::start(sup_conn, deny_tracker, network, async |cfg| {
        logging::init(cfg.log_sink, &cfg.log_filter);

        info!("setup vm");
        init::setup(
            &cfg.init_config,
            &cfg.mount_config,
            &cfg.sockets,
            cfg.nested_virt,
        )?;

        let dns = Rc::new(net::DnsState::new());
        net::start_dns(dns.clone()).await?;
        net::start_host_socket_forward(&cfg.network, cfg.sockets)?;
        net::start_host_port_forward(&cfg.init_config.host_ports, cfg.network.clone()).await?;
        net::start_tcp_proxy(cfg.network.clone(), dns)?;
        admin::start(admin_state.clone()).await?;

        if !cfg.daemons.is_empty() {
            info!("starting {} daemon(s)", cfg.daemons.len());
            let set = daemon::DaemonSet::start_all(cfg.daemons, cfg.uid, cfg.gid);
            *cfg.daemon_set_slot.borrow_mut() = Some(set);
        }

        info!("start: {} {}", cfg.cmd, cfg.args.join(" "));
        let proc = process::spawn_user(
            &cfg.cmd,
            &cfg.args,
            &cfg.env,
            &cfg.cwd,
            cfg.uid,
            cfg.gid,
            cfg.harden,
            cfg.pty_size,
        )?;
        info!("main process started");

        Ok(proc)
    })
    .await?;

    info!("main process done, exit_code = {exit_code}");

    // Keep supervisor alive until the CLI kills the VM — the main process is
    // done but sidecar `exec` processes may still be running.
    std::future::pending::<()>().await;

    Ok(())
}

/// Turn the accepted network-channel fd into a `NetworkProxy` client
/// capability. The guest is the capnp *client* side here (the host
/// serves the bootstrap `NetworkProxy`), even though the guest accepted
/// the vsock connection — vsock direction and capnp side are
/// independent.
fn bootstrap_network_client(conn_fd: OwnedFd) -> anyhow::Result<network_proxy::Client> {
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
