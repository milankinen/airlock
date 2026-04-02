use super::tls::{self, TlsInterceptor};
use ezpez_protocol::supervisor_capnp::{log_sink, network_proxy, tcp_sink};
use std::cell::RefCell;
use std::rc::Rc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use crate::rpc::HostCA;

const PROXY_PORT: u16 = 15001;

pub fn start_proxy(
    network: network_proxy::Client,
    ca: HostCA,
    log_sink: log_sink::Client,
) {
    tokio::task::spawn_local(async move {
        let logger = Logger(log_sink);
        let interceptor = match TlsInterceptor::new(&ca.cert, &ca.key) {
            Ok(i) => Rc::new(i),
            Err(e) => {
                logger.warn(&format!("TLS interceptor init failed: {e}")).await;
                return;
            }
        };

        let listener = match TcpListener::bind(("127.0.0.1", PROXY_PORT)).await {
            Ok(l) => l,
            Err(e) => {
                logger.warn(&format!("proxy listen failed: {e}")).await;
                return;
            }
        };

        logger.info(&format!("proxy listening on port {PROXY_PORT}")).await;

        loop {
            let (stream, _) = match listener.accept().await {
                Ok(s) => s,
                Err(e) => {
                    logger.warn(&format!("proxy accept: {e}")).await;
                    continue;
                }
            };

            let network = network.clone();
            let interceptor = interceptor.clone();
            let logger = logger.clone();

            tokio::task::spawn_local(async move {
                if let Err(e) = handle_connection(stream, &network, &interceptor, &logger).await {
                    logger.debug(&format!("proxy conn: {e}")).await;
                }
            });
        }
    });
}

async fn handle_connection(
    stream: tokio::net::TcpStream,
    network: &network_proxy::Client,
    interceptor: &TlsInterceptor,
    logger: &Logger,
) -> anyhow::Result<()> {
    let (orig_host, orig_port) = get_original_dst(&stream)?;

    // Peek to detect TLS
    let mut peek_buf = vec![0u8; 4096];
    let n = stream.peek(&mut peek_buf).await?;
    let is_tls = tls::is_tls(&peek_buf[..n]);

    let hostname = if is_tls {
        tls::extract_sni(&peek_buf[..n]).unwrap_or(orig_host.clone())
    } else {
        orig_host.clone()
    };

    // Channel for server→container data (bounded for backpressure)
    let (server_tx, mut server_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let server_sink: tcp_sink::Client =
        capnp_rpc::new_client(ChannelSink(RefCell::new(Some(server_tx))));

    logger
        .debug(&format!("connect {hostname}:{orig_port} tls={is_tls}"))
        .await;

    let mut req = network.connect_request();
    req.get().set_host(&hostname);
    req.get().set_port(orig_port);
    req.get().set_tls(is_tls);
    req.get().set_client(server_sink);

    let response = req.send().promise.await?;
    let client_sink = response.get()?.get_server()?;

    if is_tls {
        // TLS interception with timeout on handshake
        let tls_stream = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            interceptor.accept(stream, &hostname),
        )
        .await
        .map_err(|_| anyhow::anyhow!("TLS handshake timeout"))??;

        let (mut read, mut write) = tokio::io::split(tls_stream);
        relay(&mut read, &mut write, client_sink, &mut server_rx).await;
    } else {
        let (mut read, mut write) = stream.into_split();
        relay(&mut read, &mut write, client_sink, &mut server_rx).await;
    }

    Ok(())
}

async fn relay(
    local_read: &mut (impl AsyncReadExt + Unpin),
    local_write: &mut (impl AsyncWriteExt + Unpin),
    remote_sink: tcp_sink::Client,
    remote_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
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
                Err(_) => break,
            }
        }
        let _ = remote_sink.close_request().send().promise.await;
    };

    let to_local = async {
        while let Some(data) = remote_rx.recv().await {
            if local_write.write_all(&data).await.is_err() {
                break;
            }
        }
        let _ = local_write.shutdown().await;
    };

    tokio::join!(to_remote, to_local);
}

// -- ChannelSink: bridges RPC push into an mpsc channel --

struct ChannelSink(RefCell<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>);

impl tcp_sink::Server for ChannelSink {
    async fn send(
        self: Rc<Self>,
        params: tcp_sink::SendParams,
    ) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?;
        if let Some(tx) = self.0.borrow().as_ref() {
            // Bounded send — blocks if channel full (backpressure)
            tx.send(data.to_vec())
                .await
                .map_err(|_| capnp::Error::failed("channel closed".into()))?;
        }
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

// -- Logger: pushes log messages to host via RPC --

#[derive(Clone)]
struct Logger(log_sink::Client);

impl Logger {
    async fn debug(&self, msg: &str) {
        self.log(0, msg).await;
    }
    async fn info(&self, msg: &str) {
        self.log(1, msg).await;
    }
    async fn warn(&self, msg: &str) {
        self.log(2, msg).await;
    }
    async fn log(&self, level: u8, msg: &str) {
        let mut req = self.0.log_request();
        req.get().set_level(level);
        req.get().set_message(msg);
        let _ = req.send().await;
    }
}

// -- SO_ORIGINAL_DST --

const SO_ORIGINAL_DST: libc::c_int = 80;

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
