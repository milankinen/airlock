//! Transparent TCP proxy running inside the guest VM.
//!
//! All outbound TCP from the guest is iptables-redirected to port 15001. This
//! proxy accepts those connections, recovers the original destination via
//! `SO_ORIGINAL_DST`, reverse-maps the virtual IP to a hostname through the
//! DNS state, and forwards the connection to the host CLI via RPC.

use std::cell::RefCell;
use std::net::Ipv4Addr;
use std::rc::Rc;

use airlock_common::supervisor_capnp::{connect_result, network_proxy, tcp_sink};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tracing::{debug, info, warn};

use super::dns::DnsState;

const PROXY_PORT: u16 = 15001;

/// Spawn the transparent TCP proxy as a local task.
pub async fn start_proxy(network: network_proxy::Client, dns: Rc<DnsState>) -> anyhow::Result<()> {
    info!("start network proxy");
    let listener = TcpListener::bind(("127.0.0.1", PROXY_PORT)).await?;
    info!("network proxy listening on port {PROXY_PORT}");
    tokio::task::spawn_local(async move {
        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    warn!("proxy accept: {e}");
                    continue;
                }
            };

            let network = network.clone();
            let dns = dns.clone();
            tokio::task::spawn_local(async move {
                if let Err(e) = handle_connection(stream, &network, &dns).await {
                    debug!("proxy conn: {e}");
                }
            });
        }
    });
    Ok(())
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    network: &network_proxy::Client,
    dns: &DnsState,
) -> anyhow::Result<()> {
    let (orig_host, orig_port) = get_original_dst(&stream)?;

    // Resolve hostname via virtual DNS reverse lookup, fall back to raw IP
    let hostname = orig_host
        .parse::<Ipv4Addr>()
        .ok()
        .and_then(|ip| dns.reverse(ip))
        .unwrap_or(orig_host);

    debug!("connect {hostname}:{orig_port}");

    // Channel for server→container data
    let (server_tx, mut server_rx) = tokio::sync::mpsc::channel::<Bytes>(1);
    let server_sink: tcp_sink::Client =
        capnp_rpc::new_client(ChannelSink(RefCell::new(Some(server_tx))));

    let mut req = network.connect_request();
    let mut tcp = req.get().init_target().init_tcp();
    tcp.set_host(&hostname);
    tcp.set_port(orig_port);
    req.get().set_client(server_sink);

    let response = req.send().promise.await?;
    let result = response.get()?.get_result()?;
    let client_sink = match result.which() {
        Ok(connect_result::Server(Ok(sink))) => sink,
        Ok(connect_result::Denied(Ok(reason))) => {
            let reason = reason.to_str().unwrap_or("unknown");
            debug!("connection denied: {reason}");
            anyhow::bail!("denied: {reason}");
        }
        _ => anyhow::bail!("invalid connect result"),
    };

    // Raw TCP relay — just forward bytes both directions
    let (mut read, mut write) = stream.into_split();
    relay(&mut read, &mut write, client_sink, &mut server_rx).await;
    Ok(())
}

/// Bidirectional byte relay between a local TCP stream and a remote RPC sink.
async fn relay(
    local_read: &mut (impl AsyncReadExt + Unpin),
    local_write: &mut (impl AsyncWriteExt + Unpin),
    remote_sink: tcp_sink::Client,
    remote_rx: &mut tokio::sync::mpsc::Receiver<Bytes>,
) {
    let to_remote = async {
        let mut buf = [0u8; 8192];
        loop {
            match local_read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut req = remote_sink.send_request();
                    req.get().set_data(&buf[..n]);
                    if req.send().await.is_err() {
                        break;
                    }
                }
            }
        }
    };

    let to_local = async {
        while let Some(data) = remote_rx.recv().await {
            if local_write.write_all(&data).await.is_err() {
                break;
            }
        }
    };

    // When either direction closes, clean up both sides
    tokio::select! {
        () = to_remote => {}
        () = to_local => {}
    }
    let _ = remote_sink.close_request().send().promise.await;
    let _ = local_write.shutdown().await;
}

/// Bridges RPC `TcpSink.send()` push calls into a tokio mpsc channel.
///
/// Shared between the TCP proxy and socket forwarding modules.
pub(crate) struct ChannelSink(pub RefCell<Option<tokio::sync::mpsc::Sender<Bytes>>>);

impl tcp_sink::Server for ChannelSink {
    async fn send(self: Rc<Self>, params: tcp_sink::SendParams) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?;
        let tx = self.0.borrow().clone();
        let Some(tx) = tx.as_ref() else {
            return Err(capnp::Error::failed("channel closed".into()));
        };
        tx.send(Bytes::copy_from_slice(data))
            .await
            .map_err(|_| capnp::Error::failed("channel closed".into()))?;
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: tcp_sink::CloseParams,
        _results: tcp_sink::CloseResults,
    ) -> Result<(), capnp::Error> {
        self.0.borrow_mut().take();
        Ok(())
    }
}

const SO_ORIGINAL_DST: libc::c_int = 80;

/// Recover the original destination address from an iptables-redirected socket
/// via the `SO_ORIGINAL_DST` socket option.
#[allow(clippy::ptr_as_ptr, clippy::borrow_as_ptr, clippy::ref_as_ptr)]
fn get_original_dst(stream: &tokio::net::TcpStream) -> anyhow::Result<(String, u16)> {
    use std::os::unix::io::AsRawFd;

    let fd = stream.as_raw_fd();
    let mut addr: libc::sockaddr_in = unsafe { std::mem::zeroed() };
    let mut len: libc::socklen_t = std::mem::size_of::<libc::sockaddr_in>() as u32;

    let ret = unsafe {
        libc::getsockopt(
            fd,
            libc::IPPROTO_IP,
            SO_ORIGINAL_DST,
            &mut addr as *mut _ as *mut libc::c_void,
            &mut len,
        )
    };

    if ret != 0 {
        anyhow::bail!(
            "SO_ORIGINAL_DST failed: {}",
            std::io::Error::last_os_error()
        );
    }

    let ip = std::net::Ipv4Addr::from(u32::from_be(addr.sin_addr.s_addr));
    let port = u16::from_be(addr.sin_port);
    Ok((ip.to_string(), port))
}
