mod init;
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
    local.run_until(run()).await?;
    Ok(())
}

async fn run() -> anyhow::Result<()> {
    let listen_fd = vsock::listen(ezpez_protocol::SUPERVISOR_PORT)?;
    let conn_fd = vsock::accept(&listen_fd)?;
    drop(listen_fd);

    let exit_code = rpc::start(
        conn_fd,
        async |init_config, cmd, args, log_sink, log_filter, pty_size, network, sockets| {
            logging::init(log_sink, &log_filter);

            info!("setup vm");
            init::setup(&init_config)?;

            let dns = Rc::new(net::dns::DnsState::new());
            net::dns::start(dns.clone());
            net::start_proxy(network.clone(), dns);
            net::socket::start(&network, sockets);

            info!("start: {} {}", cmd, args.join(" "));
            let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
            let proc = process::spawn(&cmd, &args_ref, pty_size)?;
            info!("main process started");

            Ok(proc)
        },
    )
    .await?;

    info!("main process done, exit_code = {exit_code}");

    // Keep supervisor alive until the CLI kills the VM
    std::future::pending::<()>().await;

    Ok(())
}
