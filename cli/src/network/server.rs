use std::cell::RefCell;
use std::rc::Rc;

use ezpez_protocol::supervisor_capnp::{network_proxy, tcp_sink};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tracing::{debug, error, trace};

use super::scripting::host_matches;
use super::{Network, http_proxy};

fn is_localhost(host: &str) -> bool {
    host == "127.0.0.1" || host == "localhost" || host == "::1"
}

impl network_proxy::Server for Network {
    async fn connect(
        self: Rc<Self>,
        params: network_proxy::ConnectParams,
        mut results: network_proxy::ConnectResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let host = params.get_host()?.to_str()?;
        let port = params.get_port();
        let tls = params.get_tls();
        let client_sink = params.get_client()?;

        if is_localhost(host) && !self.host_ports.contains(&port) {
            debug!("blocked localhost:{port} (not in host_ports)");
            return Err(capnp::Error::failed(format!(
                "host port {port} is not exposed"
            )));
        }

        // Check if host is allowed
        if !self.script_engine.is_host_allowed(host) {
            debug!("tcp_connect denied: {host}:{port} (not in allowed_hosts)");
            results
                .get()
                .init_result()
                .set_denied("host not in allowed_hosts");
            return Ok(());
        }
        let addr = format!("{host}:{port}");
        trace!("connecting to {addr} tls={tls}");
        let stream = TcpStream::connect(&addr)
            .await
            .map_err(|e| capnp::Error::failed(format!("connect to {addr} failed: {e}")))?;

        let is_passthrough = self.tls_passthrough.iter().any(|p| host_matches(host, p));
        let http_engine = if self.script_engine.has_http_rules() && !is_passthrough {
            Some(self.script_engine.clone())
        } else {
            None
        };

        let connect = super::scripting::TcpConnect {
            host: host.to_string(),
            port,
            tls,
        };

        let sink = if tls {
            let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|e| capnp::Error::failed(format!("invalid hostname: {e}")))?;
            let tls_stream = self
                .tls
                .connect(server_name, stream)
                .await
                .map_err(|e| capnp::Error::failed(format!("TLS to {host} failed: {e}")))?;
            let alpn = tls_stream.get_ref().1.alpn_protocol();
            let server_proto = if alpn == Some(b"h2") {
                trace!("TLS established to {addr} (h2)");
                http_proxy::ServerProtocol::Http2
            } else {
                trace!("TLS established to {addr} (h1)");
                http_proxy::ServerProtocol::Http1
            };
            let (read, write) = tokio::io::split(tls_stream);
            spawn_relay(read, write, client_sink, http_engine, connect, server_proto)
        } else {
            let (read, write) = stream.into_split();
            // Non-TLS always uses h1 (h2c / plaintext h2 is not supported)
            spawn_relay(
                read,
                write,
                client_sink,
                http_engine,
                connect,
                http_proxy::ServerProtocol::Http1,
            )
        };

        results.get().init_result().set_server(sink);

        debug!("relay started: {addr} tls={tls} passthrough={is_passthrough}");
        Ok(())
    }
}

/// Spawn a relay between the container (via RPC channel) and the real server.
///
/// If `http_engine` is Some, tries to detect HTTP and intercept via hyper.
/// Falls back to raw byte relay if not HTTP or no http rules.
fn spawn_relay<R, W>(
    mut server_read: R,
    mut server_write: W,
    client_sink: tcp_sink::Client,
    http_engine: Option<Rc<super::scripting::ScriptEngine>>,
    connect: super::scripting::TcpConnect,
    server_protocol: http_proxy::ServerProtocol,
) -> tcp_sink::Client
where
    R: tokio::io::AsyncRead + Unpin + 'static,
    W: tokio::io::AsyncWrite + Unpin + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
    let error: RelayError = Rc::new(RefCell::new(None));
    let task_error = error.clone();

    tokio::task::spawn_local(async move {
        let result: Result<(), String> = async {
            // Try HTTP interception if configured
            if let Some(engine) = http_engine {
                match http_proxy::detect(&mut rx).await {
                    Ok(prefix) => {
                        http_proxy::serve(
                            prefix,
                            rx,
                            client_sink,
                            server_read,
                            server_write,
                            server_protocol,
                            &engine,
                            &connect,
                        )
                        .await
                        .map_err(|e| format!("http proxy: {e}"))?;
                        debug!("http relay closed");
                        return Ok(());
                    }
                    Err(buffered) => {
                        debug!(
                            "not HTTP ({} bytes buffered), falling back to raw relay",
                            buffered.len()
                        );
                        if !buffered.is_empty() {
                            server_write
                                .write_all(&buffered)
                                .await
                                .map_err(|e| format!("write failed: {e}"))?;
                        }
                    }
                }
            }

            // Raw bidirectional relay
            let client_sink_clone = client_sink.clone();
            tokio::task::spawn_local(async move {
                let mut buf = [0u8; 8192];
                loop {
                    match server_read.read(&mut buf).await {
                        Ok(0) => {
                            debug!("server→container: server closed");
                            break;
                        }
                        Err(e) => {
                            debug!("server→container: read error: {e}");
                            break;
                        }
                        Ok(n) => {
                            let mut req = client_sink_clone.send_request();
                            req.get().set_data(&buf[..n]);
                            if req.send().await.is_err() {
                                error!("server→container: rpc send failed");
                                break;
                            }
                        }
                    }
                }
                let _ = client_sink_clone.close_request().send().promise.await;
            });

            while let Some(data) = rx.recv().await {
                if server_write.write_all(&data).await.is_err() {
                    return Err("upstream write failed".into());
                }
            }
            debug!("container→server: channel closed");
            Ok(())
        }
        .await;

        if let Err(e) = result {
            debug!("relay error: {e}");
            *task_error.borrow_mut() = Some(e);
        }
    });

    capnp_rpc::new_client(ChannelSink::new(tx, error))
}

/// Shared error state between relay task and ChannelSink.
type RelayError = Rc<RefCell<Option<String>>>;

pub struct ChannelSink {
    tx: RefCell<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>,
    error: RelayError,
}

impl ChannelSink {
    fn new(tx: tokio::sync::mpsc::Sender<Vec<u8>>, error: RelayError) -> Self {
        Self {
            tx: RefCell::new(Some(tx)),
            error,
        }
    }
}

impl tcp_sink::Server for ChannelSink {
    async fn send(self: Rc<Self>, params: tcp_sink::SendParams) -> Result<(), capnp::Error> {
        // Check if relay task has failed
        if let Some(err) = self.error.borrow().as_ref() {
            return Err(capnp::Error::failed(err.clone()));
        }
        let data = params.get()?.get_data()?;
        let tx = self.tx.borrow().clone();
        match tx.as_ref() {
            Some(tx) => {
                tx.send(data.to_vec()).await.map_err(|_| {
                    let err = self.error.borrow();
                    let msg = err.as_deref().unwrap_or("relay closed");
                    capnp::Error::failed(msg.to_string())
                })?;
            }
            None => {
                return Err(capnp::Error::failed("channel closed".to_string()));
            }
        }
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: tcp_sink::CloseParams,
        _results: tcp_sink::CloseResults,
    ) -> Result<(), capnp::Error> {
        self.tx.borrow_mut().take();
        Ok(())
    }
}
