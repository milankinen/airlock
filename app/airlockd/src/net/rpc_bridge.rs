//! Byte streams between guest and host.
//!
//! Common parts of the guest network services. Opens connections through the
//! host network proxy, and relays bytes in both directions between a local
//! connection and the host. Also lets the host open connections to guest
//! loopback ports.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_common::network_capnp::{connect_result, network_proxy, tcp_sink};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tracing::error;

/// Open a TCP connection through the host network proxy
/// (`NetworkProxy.connect`).
/// Args:
///  - `network`: Host network proxy client
///  - `host`, `port`: TCP target
///  - `server_sink`: Sink that receives the bytes from the target
///
/// Returns:
///   Host-side sink for the bytes to the target. Error if the host denies
///   the connection or the RPC fails. Callers decide the log level: `debug`
///   for expected errors (for example a deny), `error` for unexpected ones.
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

/// Connect to `127.0.0.1:<port>` in the guest and relay the raw bytes in
/// both directions with the host.
///
/// Used by `Supervisor.openLocalTcp`: the host accepted a connection for a
/// guest service.
/// Args:
///  - `port`: Guest loopback port
///  - `client`: Host-side sink for the bytes from guest to host
///
/// Returns:
///   Sink that the host uses to send bytes into the guest connection. Error
///   if the connect fails. The caller converts it into a Cap'n Proto
///   exception.
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

/// Relay bytes in both directions between a local stream and a remote RPC
/// sink, with half-close. Returns when both directions are closed.
///
/// An EOF in one direction closes only *that* direction:
///  * When the local side stops, the remote gets EOF (`close`), but the
///    remote response still goes to the local side.
///  * When the remote stops, the local write half closes.
///
/// Args:
///  - `local_read`, `local_write`: Local stream halves
///  - `remote_sink`: Sink for the bytes to the remote
///  - `remote_rx`: Channel with the bytes from the remote
pub async fn relay(
    local_read: &mut (impl AsyncReadExt + Unpin),
    local_write: &mut (impl AsyncWriteExt + Unpin),
    remote_sink: tcp_sink::Client,
    remote_rx: &mut mpsc::Receiver<Bytes>,
) {
    let to_remote = async {
        let mut buf = vec![0u8; airlock_common::RELAY_CHUNK_SIZE];
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
        // The local side sends no more. Send EOF to the remote, but let
        // `to_local` continue to read the response. Do not cancel it.
        let _ = remote_sink.close_request().send().promise.await;
    };

    let to_local = async {
        while let Some(data) = remote_rx.recv().await {
            if let Err(e) = local_write.write_all(&data).await {
                error!("relay local write: {e}");
                break;
            }
        }
        // The remote sends no more. Send EOF to the local peer.
        let _ = local_write.shutdown().await;
    };

    // Run both directions to their end, so a one-way close is a half-close.
    // A previous version closed both directions on the first EOF. This cut
    // each "request, half-close, wait for reply" protocol (redis-style, RPC).
    tokio::join!(to_remote, to_local);
}

/// `TcpSink` server that puts the data of each RPC `send()` call into a
/// tokio mpsc channel. `close()` closes the channel.
pub struct ChannelSink {
    /// Channel sender. `None` after `close()`.
    tx: RefCell<Option<mpsc::Sender<Bytes>>>,
    /// Optional wake-up signal. Each successful `send`/`close` (and the drop)
    /// notifies it. Thus a consumer that cannot await the channel (the sync
    /// smoltcp poll loop) wakes up without polling.
    notify: Option<Rc<Notify>>,
}

impl ChannelSink {
    /// Create a sink that sends the data to `tx`.
    pub fn new(tx: mpsc::Sender<Bytes>) -> Self {
        Self {
            tx: RefCell::new(Some(tx)),
            notify: None,
        }
    }

    /// Create a sink that sends the data to `tx` and notifies `notify` after
    /// each change.
    pub fn with_notify(tx: mpsc::Sender<Bytes>, notify: Rc<Notify>) -> Self {
        Self {
            tx: RefCell::new(Some(tx)),
            notify: Some(notify),
        }
    }

    /// Notify the consumer, if there is a `notify`.
    fn wake(&self) {
        if let Some(n) = &self.notify {
            n.notify_one();
        }
    }
}

impl Drop for ChannelSink {
    /// The host can release this capability without a `close()` call (for
    /// example after a denied or failed connect, an error or a reset). Then
    /// `tx` drops without a signal. Thus wake also on drop, so the poll loop
    /// still sees the `Disconnected` result.
    fn drop(&mut self) {
        self.wake();
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
