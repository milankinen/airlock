mod net;
mod rpc;
mod vsock;
mod process;

use tokio::task::LocalSet;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let local = LocalSet::new();
    local.run_until(run()).await
}

async fn run() -> anyhow::Result<()> {
    let listen_fd = vsock::listen(ezpez_protocol::SUPERVISOR_PORT)?;
    let conn_fd = vsock::accept(&listen_fd)?;
    drop(listen_fd);

    let conn = rpc::connect(conn_fd).await?;

    net::start_proxy(conn.network, conn.ca, conn.log_sink);

    let proc = process::spawn("crun", &["run", "--no-pivot", "--bundle", "/mnt/bundle", "ezpez0"])?;
    proc.attach(conn.proc).await;

    // Keep supervisor alive until the CLI kills the VM
    std::future::pending::<()>().await;

    Ok(())
}
