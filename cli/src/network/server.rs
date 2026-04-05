use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;

use bytes::Bytes;
use ezpez_protocol::supervisor_capnp::{network_proxy, tcp_sink};
use tokio::sync::mpsc;
use tracing::debug;

use super::scripting::host_matches;
use super::{Network, http, io, tcp, tls};

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
        let host = params.get_host()?.to_str()?.to_string();
        let port = params.get_port();
        let client_sink = params.get_client()?;

        if is_localhost(&host) && !self.host_ports.contains(&port) {
            debug!("blocked localhost:{port} (not in host_ports)");
            return Err(capnp::Error::failed(format!(
                "host port {port} is not exposed"
            )));
        }

        if !self.script_engine.is_host_allowed(&host) {
            debug!("denied: {host}:{port} (not in allowed_hosts)");
            results
                .get()
                .init_result()
                .set_denied("host not in allowed_hosts");
            return Ok(());
        }

        debug!("connect {host}:{port}");

        let sink = spawn_connection(
            host,
            port,
            client_sink,
            self.tls_client.clone(),
            self.interceptor.clone(),
            self.tls_passthrough.clone(),
            self.script_engine.clone(),
        );
        results.get().init_result().set_server(sink);
        Ok(())
    }
}

#[allow(clippy::too_many_arguments)]
fn spawn_connection(
    host: String,
    port: u16,
    client_sink: tcp_sink::Client,
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    tls_passthrough: Vec<String>,
    script_engine: Rc<super::scripting::ScriptEngine>,
) -> tcp_sink::Client {
    let (tx, rx) = mpsc::channel::<Bytes>(1);
    let error: io::RelayError = Rc::new(RefCell::new(None));
    let task_error = error.clone();

    tokio::task::spawn_local(async move {
        let result = Box::pin(handle_connection(
            &host,
            port,
            rx,
            client_sink,
            &tls_client,
            &interceptor,
            &tls_passthrough,
            &script_engine,
        ))
        .await;

        if let Err(e) = result {
            debug!("connection {host}:{port} error: {e}");
            *task_error.borrow_mut() = Some(format!("{e}"));
        }
    });

    capnp_rpc::new_client(io::ChannelSink::new(tx, error))
}

#[allow(clippy::too_many_arguments)]
async fn handle_connection(
    host: &str,
    port: u16,
    mut rx: mpsc::Receiver<Bytes>,
    client_sink: tcp_sink::Client,
    tls_client: &Arc<rustls::ClientConfig>,
    interceptor: &tls::TlsInterceptor,
    tls_passthrough: &[String],
    script_engine: &Rc<super::scripting::ScriptEngine>,
) -> anyhow::Result<()> {
    let (is_tls, first) = tls::detect(&mut rx).await;
    let addr = format!("{host}:{port}");

    // TLS passthrough: raw relay, container↔server TLS end-to-end
    if is_tls && tls_passthrough.iter().any(|p| host_matches(host, p)) {
        debug!("passthrough: {addr}");
        let (container, server) = tcp::establish(&addr, first, rx, client_sink).await?;
        Box::pin(tcp::relay(container, server)).await;
        return Ok(());
    }

    // Establish connection pair
    let (container, server) = if is_tls {
        tls::establish(host, port, first, rx, client_sink, interceptor, tls_client).await?
    } else {
        tcp::establish(&addr, first, rx, client_sink).await?
    };

    let connect = super::scripting::TcpConnect {
        host: host.to_string(),
        port,
        tls: is_tls,
    };

    // Detect HTTP if rules are configured
    let (container, is_http) = if script_engine.has_http_rules() {
        detect_http(container).await
    } else {
        (container, false)
    };

    if is_http {
        Box::pin(http::relay(container, server, script_engine, connect)).await?;
        Ok(())
    } else {
        Box::pin(tcp::relay(container, server)).await;
        Ok(())
    }
}

/// Peek at the container stream to detect HTTP.
async fn detect_http(mut container: io::Transport) -> (io::Transport, bool) {
    match http::detect(&mut container.read).await {
        Ok(prefix) => {
            container.read = Box::new(io::PrefixedRead::new(prefix, container.read));
            (container, true)
        }
        Err(buffered) => {
            container.read = Box::new(io::PrefixedRead::new(buffered, container.read));
            (container, false)
        }
    }
}
