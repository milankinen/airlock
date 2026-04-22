//! In-VM supervisor process.
//!
//! Runs as PID 1 inside the guest Linux VM. Listens on a virtio-vsock port for
//! a single Cap'n Proto RPC connection from the host CLI, then bootstraps the
//! guest environment (mounts, networking, DNS) and spawns the user's command.

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

use std::rc::Rc;

use tokio::task::LocalSet;
use tracing::info;

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::large_futures)]
async fn main() -> anyhow::Result<()> {
    let local = LocalSet::new();
    local.run_until(run()).await?;
    Ok(())
}

/// Single-connection lifecycle: accept the host CLI connection, set up the
/// guest, run the user's process, then idle until the VM is torn down.
async fn run() -> anyhow::Result<()> {
    let listen_fd = vsock::listen(airlock_common::SUPERVISOR_PORT)?;
    let conn_fd = vsock::accept(&listen_fd)?;
    drop(listen_fd);

    let admin_state = admin::AdminState::new();
    let deny_tracker = admin_state.deny_tracker.clone();

    let exit_code = rpc::start(conn_fd, deny_tracker, async |cfg| {
        logging::init(cfg.log_sink, &cfg.log_filter);

        info!("setup vm");
        init::setup(
            &cfg.init_config,
            &cfg.mount_config,
            &cfg.sockets,
            cfg.nested_virt,
        )?;

        let dns = Rc::new(net::dns::DnsState::new());
        net::dns::start(dns.clone()).await?;
        net::host_socket_forward::start(&cfg.network, cfg.sockets)?;
        net::host_port_forward::start(&cfg.init_config.host_ports, cfg.network.clone()).await?;
        net::tcp_proxy::start(cfg.network.clone(), dns)?;
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
