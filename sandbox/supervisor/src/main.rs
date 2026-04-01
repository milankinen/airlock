mod rpc;
mod vsock;

use tokio::task::LocalSet;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    eprintln!("supervisor: starting on vsock port {}", ezpez_protocol::SUPERVISOR_PORT);

    let listen_fd = vsock::listen(ezpez_protocol::SUPERVISOR_PORT)?;
    eprintln!("supervisor: listening");

    let conn_fd = vsock::accept(&listen_fd)?;
    drop(listen_fd);
    eprintln!("supervisor: host connected");

    let local = LocalSet::new();
    local.run_until(rpc::serve(conn_fd)).await?;

    eprintln!("supervisor: disconnected, exiting");
    Ok(())
}
