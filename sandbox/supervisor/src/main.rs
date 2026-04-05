mod logging;
mod net;
mod process;
mod rpc;
mod vsock;

use std::rc::Rc;

use tokio::task::LocalSet;
use tracing::info;

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::large_futures)]
async fn main() -> anyhow::Result<()> {
    let local = LocalSet::new();
    local.run_until(run()).await
}

async fn run() -> anyhow::Result<()> {
    let listen_fd = vsock::listen(ezpez_protocol::SUPERVISOR_PORT)?;
    let conn_fd = vsock::accept(&listen_fd)?;
    drop(listen_fd);

    let conn = rpc::connect(conn_fd).await?;
    logging::init(conn.log_sink, &conn.log_filter);

    let dns = Rc::new(net::dns::DnsState::new());
    net::dns::start(dns.clone());
    net::start_proxy(conn.network, dns);

    // Create cache volume subdirs (the ext4 volume is already mounted at /mnt/cache by init)
    for dir in &conn.cache_dirs {
        let path = format!("/mnt/cache/{dir}");
        if let Err(e) = std::fs::create_dir_all(&path) {
            tracing::warn!("failed to create cache dir {path}: {e}");
        } else {
            info!("cache dir: /mnt/cache/{dir}");
        }
    }

    info!("start: {} {}", conn.cmd, conn.args.join(" "));
    let args_ref: Vec<&str> = conn.args.iter().map(String::as_str).collect();
    let proc = process::spawn(&conn.cmd, &args_ref, conn.proc.pty_size)?;
    let exit_code = proc.attach(conn.proc).await;
    info!("main process done, exit_code = {exit_code}");

    // Keep supervisor alive until the CLI kills the VM
    std::future::pending::<()>().await;

    Ok(())
}
