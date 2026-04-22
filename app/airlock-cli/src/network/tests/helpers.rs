use std::collections::BTreeMap;
use std::future::Future;
use std::net::SocketAddr;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::task::{Context, Poll};

use airlock_common::network_capnp::{connect_result, network_proxy, tcp_sink};
use axum::Router;
use bytes::{Buf, Bytes};
use capnp_rpc::{rpc_twoparty_capnp, twoparty};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio::task::LocalSet;

use crate::config::config::{self, MiddlewareRule, NetworkRule, Policy};
use crate::network::middleware::LogFn;
use crate::network::tls::TlsInterceptor;
use crate::network::{Network, NetworkState, rules};

/// Collects log messages from Lua `log()` calls for test assertions.
#[derive(Clone)]
pub struct RequestLog(Rc<std::cell::RefCell<Vec<String>>>);

impl RequestLog {
    pub fn new() -> (Self, LogFn) {
        let log = Self(Rc::new(std::cell::RefCell::new(Vec::new())));
        let inner = log.0.clone();
        let log_fn: LogFn = Rc::new(move |msg: &str| inner.borrow_mut().push(msg.to_string()));
        (log, log_fn)
    }

    pub fn messages(&self) -> Vec<String> {
        self.0.borrow().clone()
    }
}

// ── Test HTTP server ────────────────────────────────────

pub async fn serve(app: Router) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    addr
}

// ── Network + RPC harness ───────────────────────────────

/// Test network configuration
pub struct TestNetworkConfig {
    pub allowed_hosts: Vec<String>,
    pub middleware_scripts: Vec<(&'static str, &'static str)>,
    /// Extra CA PEMs to trust (e.g. test server CAs)
    pub trust_cas: Vec<String>,
}

impl Default for TestNetworkConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: vec!["*".into()],
            middleware_scripts: vec![],
            trust_cas: vec![],
        }
    }
}

pub fn run_network<F, Fut>(
    allowed_hosts: Vec<String>,
    middleware_scripts: Vec<(&'static str, &'static str)>,
    f: F,
) where
    F: FnOnce(network_proxy::Client) -> Fut,
    Fut: Future<Output = ()>,
{
    run_network_with_log(allowed_hosts, middleware_scripts, |proxy, _log| f(proxy));
}

pub fn run_network_with_log<F, Fut>(
    allowed_hosts: Vec<String>,
    middleware_scripts: Vec<(&'static str, &'static str)>,
    f: F,
) where
    F: FnOnce(network_proxy::Client, RequestLog) -> Fut,
    Fut: Future<Output = ()>,
{
    run_with_config(
        TestNetworkConfig {
            allowed_hosts,
            middleware_scripts,
            ..Default::default()
        },
        |proxy, log, _| f(proxy, log),
    );
}

/// Shortcut: `run_network` with `allowed_hosts=["*"]` and no middleware.
pub fn run_plain<F, Fut>(f: F)
where
    F: FnOnce(network_proxy::Client) -> Fut,
    Fut: Future<Output = ()>,
{
    run_network(vec!["*".into()], vec![], f);
}

/// Full test runner: provides proxy, request log, and the MITM CA PEM
/// (for container-side TLS clients to trust).
pub fn run_with_config<F, Fut>(cfg: TestNetworkConfig, f: F)
where
    F: FnOnce(network_proxy::Client, RequestLog, String /* mitm_ca_pem */) -> Fut,
    Fut: Future<Output = ()>,
{
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let local = LocalSet::new();
    rt.block_on(local.run_until(async move {
        let (log, mitm_ca_pem, network) = build_network(cfg);
        let proxy = start_rpc(network);
        f(proxy, log, mitm_ca_pem).await;
    }));
}

fn build_network(cfg: TestNetworkConfig) -> (RequestLog, String, Network) {
    // Build rules from test config (no middleware — rules are pure allow/deny).
    let mut rules = BTreeMap::new();
    let middleware_targets = cfg.allowed_hosts.clone();

    // Main allow rule
    if !cfg.allowed_hosts.is_empty() {
        rules.insert(
            "test-allow".to_string(),
            NetworkRule {
                enabled: true,
                allow: cfg.allowed_hosts,
                deny: vec![],
                passthrough: false,
            },
        );
    }

    // Build middleware from test config. Middleware targets default to
    // allowed_hosts. MITM is always on for allowed targets regardless of
    // middleware presence, so no synthetic no-op middleware is needed.
    let mut middleware_config = BTreeMap::new();

    for (i, (_, script)) in cfg.middleware_scripts.iter().enumerate() {
        middleware_config.insert(
            format!("test-mw-{i}"),
            MiddlewareRule {
                enabled: true,
                target: middleware_targets.clone(),
                env: BTreeMap::new(),
                script: script.to_string(),
            },
        );
    }

    // Tests use a deny-by-default model: only explicitly listed hosts are permitted.
    let config = config::Network {
        policy: Policy::DenyByDefault,
        rules,
        middleware: middleware_config,
        ports: BTreeMap::default(),
        sockets: BTreeMap::default(),
    };
    let (request_log, log_fn) = RequestLog::new();
    let rule_targets = rules::resolve(&config);
    // Tests don't need real secret storage — a disabled vault gives
    // the substitution machinery a no-op backend and never prompts.
    let vault = crate::vault::Vault::for_storage_type(crate::vault::VaultStorageType::Disabled);
    let middleware_targets = rules::resolve_middleware(&config, &vault, &log_fn).unwrap();

    // MITM CA
    let mitm_ca_key = rcgen::KeyPair::generate().unwrap();
    let mitm_ca_params = rcgen::CertificateParams::new(vec![]).unwrap();
    let mitm_ca_cert = mitm_ca_params.self_signed(&mitm_ca_key).unwrap();
    let mitm_ca_pem = mitm_ca_cert.pem();
    let interceptor = TlsInterceptor::new(&mitm_ca_pem, &mitm_ca_key.serialize_pem()).unwrap();

    // TLS client: trust system roots + extra test CAs
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_native_certs::load_native_certs().expect("native certs") {
        let _ = root_store.add(cert);
    }
    for ca_pem in &cfg.trust_cas {
        for cert in rustls_pemfile::certs(&mut ca_pem.as_bytes()) {
            let _ = root_store.add(cert.unwrap());
        }
    }
    let tls_client = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    (
        request_log,
        mitm_ca_pem,
        Network {
            state: Arc::new(parking_lot::RwLock::new(NetworkState {
                policy: Policy::DenyByDefault,
            })),
            tls_client: Arc::new(tls_client),
            interceptor: Rc::new(interceptor),
            allow_targets: rule_targets.allow,
            deny_targets: rule_targets.deny,
            passthrough_targets: rule_targets.passthrough,
            middleware_targets,
            port_forwards: std::collections::HashMap::default(),
            socket_map: std::collections::HashMap::default(),
            events: tokio::sync::broadcast::channel(1).0,
            next_id: std::sync::atomic::AtomicU64::new(0),
            deny_reporter: crate::network::DenyReporter::new(),
        },
    )
}

fn start_rpc(network: Network) -> network_proxy::Client {
    let (client_stream, server_stream) = tokio::io::duplex(4096);

    let (sr, sw) = tokio::io::split(server_stream);
    let sr = tokio_util::compat::TokioAsyncReadCompatExt::compat(sr);
    let sw = tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(sw);
    let server_network = twoparty::VatNetwork::new(
        sr,
        sw,
        rpc_twoparty_capnp::Side::Server,
        capnp::message::ReaderOptions::default(),
    );
    let server: network_proxy::Client = capnp_rpc::new_client(network);
    let rpc_system = capnp_rpc::RpcSystem::new(Box::new(server_network), Some(server.client));
    tokio::task::spawn_local(rpc_system);

    let (cr, cw) = tokio::io::split(client_stream);
    let cr = tokio_util::compat::TokioAsyncReadCompatExt::compat(cr);
    let cw = tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(cw);
    let client_network = twoparty::VatNetwork::new(
        cr,
        cw,
        rpc_twoparty_capnp::Side::Client,
        capnp::message::ReaderOptions::default(),
    );
    let mut rpc_system = capnp_rpc::RpcSystem::new(Box::new(client_network), None);
    let proxy: network_proxy::Client = rpc_system.bootstrap(rpc_twoparty_capnp::Side::Server);
    tokio::task::spawn_local(rpc_system);

    proxy
}

// ── Test connection ─────────────────────────────────────

pub struct TestConnection {
    server_sink: tcp_sink::Client,
    pub container_rx: mpsc::Receiver<Bytes>,
}

impl TestConnection {
    pub async fn connect(proxy: &network_proxy::Client, host: &str, port: u16) -> Option<Self> {
        let (tx, container_rx) = mpsc::channel::<Bytes>(16);
        let client_sink: tcp_sink::Client = capnp_rpc::new_client(CollectorSink(tx));

        let mut req = proxy.connect_request();
        let mut tcp = req.get().init_target().init_tcp();
        tcp.set_host(host);
        tcp.set_port(port);
        req.get().set_client(client_sink);

        let response = req.send().promise.await.unwrap();
        let result = response.get().unwrap().get_result().unwrap();
        match result.which().unwrap() {
            connect_result::Server(Ok(server_sink)) => Some(TestConnection {
                server_sink,
                container_rx,
            }),
            connect_result::Denied(_) => None,
            connect_result::Server(Err(e)) => panic!("connect error: {e}"),
        }
    }

    pub async fn send(&self, data: &[u8]) {
        let mut req = self.server_sink.send_request();
        req.get().set_data(data);
        req.send().await.unwrap();
    }

    pub async fn roundtrip(&mut self, request: &str) -> String {
        self.send(request.as_bytes()).await;
        self.recv(3000).await
    }

    pub async fn recv(&mut self, timeout_ms: u64) -> String {
        let mut buf = bytes::BytesMut::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
        loop {
            tokio::select! {
                data = self.container_rx.recv() => {
                    match data {
                        Some(chunk) => buf.extend_from_slice(&chunk),
                        None => break,
                    }
                }
                () = tokio::time::sleep_until(deadline) => break,
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    }

    /// Convert this connection into an AsyncRead + AsyncWrite stream.
    /// Used for TLS tests where the container needs to do a TLS handshake
    /// through the RPC channel.
    pub fn into_stream(self) -> RpcStream {
        RpcStream {
            tx: self.server_sink,
            rx: self.container_rx,
            pending: Bytes::new(),
        }
    }
}

/// AsyncRead + AsyncWrite over the RPC channel (container side).
/// Allows the test container to do TLS through the proxy.
pub struct RpcStream {
    tx: tcp_sink::Client,
    rx: mpsc::Receiver<Bytes>,
    pending: Bytes,
}

impl AsyncRead for RpcStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.pending.is_empty() {
            let n = self.pending.len().min(buf.remaining());
            buf.put_slice(&self.pending[..n]);
            self.pending.advance(n);
            return Poll::Ready(Ok(()));
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(mut data)) => {
                let n = data.len().min(buf.remaining());
                buf.put_slice(&data[..n]);
                data.advance(n);
                if !data.is_empty() {
                    self.pending = data;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for RpcStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let mut req = self.tx.send_request();
        req.get().set_data(buf);
        drop(req.send());
        Poll::Ready(Ok(buf.len()))
    }
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

struct CollectorSink(mpsc::Sender<Bytes>);

impl tcp_sink::Server for CollectorSink {
    async fn send(self: Rc<Self>, params: tcp_sink::SendParams) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?;
        let _ = self.0.send(Bytes::copy_from_slice(data)).await;
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: tcp_sink::CloseParams,
        _results: tcp_sink::CloseResults,
    ) -> Result<(), capnp::Error> {
        Ok(())
    }
}

// ── Request builders ────────────────────────────────────

pub fn http_get(port: u16, path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
}

pub fn http_post(port: u16, path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
