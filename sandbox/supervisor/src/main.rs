mod vsock;

use capnp_rpc::{rpc_twoparty_capnp, twoparty, RpcSystem};
use ezpez_protocol::supervisor_capnp::supervisor;
use futures::AsyncReadExt;
use std::os::unix::io::FromRawFd;

struct SupervisorImpl {
    ping_count: std::cell::Cell<u32>,
}

impl supervisor::Server for SupervisorImpl {
    async fn ping(
        self: capnp::capability::Rc<Self>,
        _params: supervisor::PingParams,
        mut results: supervisor::PingResults,
    ) -> Result<(), capnp::Error> {
        let id = self.ping_count.get();
        self.ping_count.set(id + 1);
        results.get().set_id(id);
        Ok(())
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    eprintln!("supervisor: starting on vsock port {}", ezpez_protocol::SUPERVISOR_PORT);

    let listen_fd = vsock::listen(ezpez_protocol::SUPERVISOR_PORT)?;
    eprintln!("supervisor: listening");

    let conn_fd = vsock::accept(listen_fd)?;
    unsafe { libc::close(listen_fd); }
    eprintln!("supervisor: host connected");

    // Wrap the raw vsock fd in an async stream via tokio
    let std_stream = unsafe { std::net::TcpStream::from_raw_fd(conn_fd) };
    std_stream.set_nonblocking(true)?;
    let stream = tokio::net::TcpStream::from_std(std_stream)?;
    let (reader, writer) = tokio_util::compat::TokioAsyncReadCompatExt::compat(stream).split();

    let network = twoparty::VatNetwork::new(
        reader,
        writer,
        rpc_twoparty_capnp::Side::Server,
        Default::default(),
    );

    let supervisor_client: supervisor::Client =
        capnp_rpc::new_client(SupervisorImpl {
            ping_count: std::cell::Cell::new(0),
        });

    let rpc = RpcSystem::new(Box::new(network), Some(supervisor_client.client));
    rpc.await?;

    eprintln!("supervisor: host disconnected, exiting");
    Ok(())
}
