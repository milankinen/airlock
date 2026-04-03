use std::cell::RefCell;
use capnp::capability::Rc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use ezpez_protocol::supervisor_capnp::{network_proxy, tcp_sink};
use super::Network;

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
            return Err(capnp::Error::failed(
                format!("host port {port} is not exposed"),
            ));
        }

        let addr = format!("{host}:{port}");

        let stream = TcpStream::connect(&addr).await.map_err(|e| {
            capnp::Error::failed(format!("connect to {addr} failed: {e}"))
        })?;

        if tls {
            let server_name = rustls::pki_types::ServerName::try_from(host.to_string())
                .map_err(|e| capnp::Error::failed(format!("invalid hostname: {e}")))?;
            let tls_stream = self.tls.connect(server_name, stream).await
                .map_err(|e| capnp::Error::failed(format!("TLS to {host} failed: {e}")))?;

            let (read, write) = tokio::io::split(tls_stream);
            let server_sink = spawn_relay(read, write, client_sink);
            results.get().set_server(server_sink);
        } else {
            let (read, write) = stream.into_split();
            let server_sink = spawn_relay(read, write, client_sink);
            results.get().set_server(server_sink);
        }

        Ok(())
    }
}

fn spawn_relay<R, W>(
    mut read: R,
    mut write: W,
    client_sink: tcp_sink::Client,
) -> tcp_sink::Client
where
    R: tokio::io::AsyncRead + Unpin + 'static,
    W: tokio::io::AsyncWrite + Unpin + 'static,
{
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);

    tokio::task::spawn_local(async move {
        let mut buf = [0u8; 8192];
        loop {
            match read.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let mut req = client_sink.send_request();
                    req.get().set_data(&buf[..n]);
                    if req.send().await.is_err() {
                        break;
                    }
                }
            }
        }
        let _ = client_sink.close_request().send().promise.await;
    });

    tokio::task::spawn_local(async move {
        while let Some(data) = rx.recv().await {
            if write.write_all(&data).await.is_err() {
                break;
            }
        }
    });

    capnp_rpc::new_client(ChannelSink(RefCell::new(Some(tx))))
}

struct ChannelSink(RefCell<Option<tokio::sync::mpsc::Sender<Vec<u8>>>>);

impl tcp_sink::Server for ChannelSink {
    async fn send(
        self: Rc<Self>,
        params: tcp_sink::SendParams,
    ) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?;
        if let Some(tx) = self.0.borrow().as_ref() {
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
