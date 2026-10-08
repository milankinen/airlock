use capnp::capability::FromClientHook;
use capnp_rpc::rpc_twoparty_capnp::Side;
use capnp_rpc::{RpcSystem, twoparty};
use tokio_util::compat::{TokioAsyncReadCompatExt, TokioAsyncWriteCompatExt};

/// Serve `server` over an in-memory two-party RPC connection and return
/// the client end, as the other side of a real transport sees it. Call
/// inside a `LocalSet`.
pub fn rpc_loopback<C: FromClientHook>(server: capnp::capability::Client) -> C {
    let (client_stream, server_stream) = tokio::io::duplex(4096);

    let (r, w) = tokio::io::split(server_stream);
    let network = twoparty::VatNetwork::new(
        r.compat(),
        w.compat_write(),
        Side::Server,
        capnp::message::ReaderOptions::default(),
    );
    tokio::task::spawn_local(RpcSystem::new(Box::new(network), Some(server)));

    let (r, w) = tokio::io::split(client_stream);
    let network = twoparty::VatNetwork::new(
        r.compat(),
        w.compat_write(),
        Side::Client,
        capnp::message::ReaderOptions::default(),
    );
    let mut rpc = RpcSystem::new(Box::new(network), None);
    let client = rpc.bootstrap(Side::Server);
    tokio::task::spawn_local(rpc);
    client
}
