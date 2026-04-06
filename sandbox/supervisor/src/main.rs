mod init;
mod logging;
mod net;
mod process;
mod rpc;
mod vsock;

use std::rc::Rc;

use tokio::task::LocalSet;
use tracing::{error, info};

#[tokio::main(flavor = "current_thread")]
#[allow(clippy::large_futures)]
async fn main() -> anyhow::Result<()> {
    let local = LocalSet::new();
    local.run_until(run()).await?;
    // Keep supervisor alive until the CLI kills the VM
    std::future::pending::<()>().await;
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let listen_fd = vsock::listen(ezpez_protocol::SUPERVISOR_PORT)?;
    let conn_fd = vsock::accept(&listen_fd)?;
    drop(listen_fd);

    let (log_sink, log_filter, mut conn) = rpc::connect(conn_fd).await?;
    logging::init(log_sink, &log_filter);

    if let Err(e) = init::setup(&conn.init_config) {
        error!("startup failed: {e:#}");
        if let Some(tx) = conn.proc.result.take() {
            let _ = tx.send(Err(format!("{e:#}")));
        }
        return Ok(());
    }

    let dns = Rc::new(net::dns::DnsState::new());
    net::dns::start(dns.clone());
    net::start_proxy(conn.network, dns);

    info!("start: {} {}", conn.cmd, conn.args.join(" "));
    let args_ref: Vec<&str> = conn.args.iter().map(String::as_str).collect();
    match process::spawn(&conn.cmd, &args_ref, conn.proc.pty_size) {
        Ok(proc) => {
            tokio::task::spawn_local(async move {
                let exit_code = proc.attach(conn.proc).await;
                info!("main process done, exit_code = {exit_code}");
            });
        }
        Err(e) => {
            error!("process spawn failed: {e:#}");
            if let Some(tx) = conn.proc.result.take() {
                let _ = tx.send(Err(format!("{e:#}")));
            }
        }
    }
    Ok(())
}
