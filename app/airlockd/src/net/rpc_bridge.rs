//! Shared Cap'n Proto sink/relay/connect plumbing.
//!
//! The guest-side networking stack has three distinct users of the
//! `NetworkProxy.connect` RPC (tcp_proxy, host_port_forward,
//! host_socket_forward) plus one reverse-direction caller
//! (`Supervisor.openLocalTcp` for host → guest). They all need the
//! same three pieces:
//!
//! - [`ChannelSink`] — a `TcpSink` server implementation that pushes
//!   inbound RPC bytes into an mpsc channel, optionally pinging a
//!   `Notify` so a sync consumer (the smoltcp poll loop) can wake up
//!   without polling.
//! - [`rpc_connect_tcp`] — builds + sends a `connect` request, unwraps
//!   the response, and returns the host-side sink.
//! - [`relay`] — bidirectional byte pump between a tokio
//!   `AsyncRead`/`AsyncWrite` pair and an RPC sink/channel pair.
//! - [`open_local_tcp`] — for the `Supervisor.openLocalTcp` RPC
//!   handler: connect to an in-guest loopback port and relay.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_common::supervisor_capnp::{connect_result, network_proxy, tcp_sink};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tracing::error;

/// Send a `NetworkProxy.connect` request for a TCP target and return
/// the host-side sink. Callers decide whether to log errors at `debug`
/// (expected; e.g. remote denied) or `error` (unexpected).
pub async fn rpc_connect_tcp(
    network: &network_proxy::Client,
    host: &str,
    port: u16,
    server_sink: tcp_sink::Client,
) -> anyhow::Result<tcp_sink::Client> {
    let mut req = network.connect_request();
    {
        let mut tcp = req.get().init_target().init_tcp();
        tcp.set_host(host);
        tcp.set_port(port);
    }
    req.get().set_client(server_sink);

    let response = req.send().promise.await?;
    let result = response.get()?.get_result()?;
    match result.which()? {
        connect_result::Server(sink) => Ok(sink?),
        connect_result::Denied(reason) => {
            let reason = reason?.to_str().unwrap_or("unknown");
            anyhow::bail!("denied: {reason}");
        }
    }
}

/// Open a local TCP connection inside the guest and bridge it to the
/// host via two sinks. Used by `Supervisor.openLocalTcp`: the host has
/// accepted a connection destined for a guest service; we connect to
/// `127.0.0.1:<port>` here and relay bytes raw in both directions.
///
/// `client` is the host-side sink (guest → host bytes). The returned
/// sink is what the host uses to push bytes into the guest's local TCP
/// connection. A connect failure surfaces as an error the caller turns
/// into a Cap'n Proto exception.
pub async fn open_local_tcp(
    port: u16,
    client: tcp_sink::Client,
) -> anyhow::Result<tcp_sink::Client> {
    let stream = tokio::net::TcpStream::connect(("127.0.0.1", port)).await?;

    let (server_tx, mut server_rx) = mpsc::channel::<Bytes>(1);
    let server_sink: tcp_sink::Client = capnp_rpc::new_client(ChannelSink::new(server_tx));

    tokio::task::spawn_local(async move {
        let (mut read, mut write) = stream.into_split();
        relay(&mut read, &mut write, client, &mut server_rx).await;
    });

    Ok(server_sink)
}

/// Bidirectional byte relay between a local TCP stream and a remote
/// RPC sink. Whichever direction ends first tears down the other:
/// the client sink is closed and the local write half is shut down.
pub async fn relay(
    local_read: &mut (impl AsyncReadExt + Unpin),
    local_write: &mut (impl AsyncWriteExt + Unpin),
    remote_sink: tcp_sink::Client,
    remote_rx: &mut mpsc::Receiver<Bytes>,
) {
    let to_remote = async {
        let mut buf = [0u8; 8192];
        loop {
            match local_read.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => {
                    let mut req = remote_sink.send_request();
                    req.get().set_data(&buf[..n]);
                    if req.send().await.is_err() {
                        break;
                    }
                }
                Err(e) => {
                    error!("relay local read: {e}");
                    break;
                }
            }
        }
    };

    let to_local = async {
        while let Some(data) = remote_rx.recv().await {
            if let Err(e) = local_write.write_all(&data).await {
                error!("relay local write: {e}");
                break;
            }
        }
    };

    // When either direction closes, tear down both sides.
    tokio::select! {
        () = to_remote => {}
        () = to_local => {}
    }
    let _ = remote_sink.close_request().send().promise.await;
    let _ = local_write.shutdown().await;
}

/// Bridges RPC `TcpSink.send()` push calls into a tokio mpsc channel.
///
/// When a `notify` is attached, every successful `send`/`close` pings
/// it so a consumer that can't await the channel directly (notably the
/// sync smoltcp poll loop) can be woken without polling.
pub struct ChannelSink {
    tx: RefCell<Option<mpsc::Sender<Bytes>>>,
    notify: Option<Rc<Notify>>,
}

impl ChannelSink {
    pub fn new(tx: mpsc::Sender<Bytes>) -> Self {
        Self {
            tx: RefCell::new(Some(tx)),
            notify: None,
        }
    }

    pub fn with_notify(tx: mpsc::Sender<Bytes>, notify: Rc<Notify>) -> Self {
        Self {
            tx: RefCell::new(Some(tx)),
            notify: Some(notify),
        }
    }

    fn wake(&self) {
        if let Some(n) = &self.notify {
            n.notify_one();
        }
    }
}

impl tcp_sink::Server for ChannelSink {
    async fn send(self: Rc<Self>, params: tcp_sink::SendParams) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?;
        let tx = self.tx.borrow().clone();
        let Some(tx) = tx.as_ref() else {
            return Err(capnp::Error::failed("channel closed".into()));
        };
        tx.send(Bytes::copy_from_slice(data))
            .await
            .map_err(|_| capnp::Error::failed("channel closed".into()))?;
        self.wake();
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: tcp_sink::CloseParams,
        _results: tcp_sink::CloseResults,
    ) -> Result<(), capnp::Error> {
        self.tx.borrow_mut().take();
        self.wake();
        Ok(())
    }
}
