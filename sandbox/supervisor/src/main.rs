mod logging;
mod net;
mod rpc;
mod vsock;
mod process;

use std::rc::Rc;
use tokio::task::LocalSet;
use tracing::info;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let local = LocalSet::new();
    local.run_until(run()).await
}

async fn run() -> anyhow::Result<()> {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    let listen_fd = vsock::listen(ezpez_protocol::SUPERVISOR_PORT)?;
    let conn_fd = vsock::accept(&listen_fd)?;
    drop(listen_fd);

    let conn = rpc::connect(conn_fd).await?;

    logging::init(conn.log_sink, conn.verbose);

    let dns = Rc::new(net::dns::DnsState::new());
    net::dns::start(dns.clone());
    net::start_proxy(conn.network, conn.ca, dns);

    info!("start main process");
    let use_pty = conn.proc.pty_size.is_some();
    let proc = process::spawn("crun", &["run", "--no-pivot", "--bundle", "/mnt/bundle", "ezpez0"], use_pty)?;
    let exit_code = proc.attach(conn.proc).await;
    info!("main process done, exit_code = {exit_code}");

    // Keep supervisor alive until the CLI kills the VM
    std::future::pending::<()>().await;

    Ok(())
}
