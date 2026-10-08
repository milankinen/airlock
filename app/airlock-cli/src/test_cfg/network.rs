//! The network harness: a [`Network`] built from a small test config,
//! served over in-memory Cap'n Proto RPC, and a container-side connection
//! through it.

use std::collections::BTreeMap;
use std::future::Future;
use std::net::TcpListener as StdListener;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::task::{Context, Poll};

use airlock_common::network_capnp::{connect_result, network_proxy, tcp_sink};
use airlock_test_utils::{TestCa, block_on_local, rpc_loopback, temp_dir, tls_trusting};
use bytes::{Buf, Bytes};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf};
use tokio::sync::{broadcast, mpsc};

use crate::config::config_values::{self, MiddlewareRule, NetworkRule, Policy};
use crate::network::interceptor::Interceptor;
use crate::network::middleware::LogFn;
use crate::network::tls::TlsInterceptor;
use crate::network::{Network, NetworkState, rules};
use crate::project::{MaskedSecret, Project, SandboxEnv};

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

// ── Network + RPC harness ───────────────────────────────

/// Test network configuration
pub struct TestNetworkConfig {
    pub allowed_hosts: Vec<String>,
    pub middleware_scripts: Vec<(&'static str, &'static str)>,
    /// Extra CA PEMs to trust (e.g. test server CAs)
    pub trust_cas: Vec<String>,
    /// Masked secrets injected by the `test-allow` rule (all allowed hosts).
    pub inject: Vec<MaskedSecret>,
    /// Hosts allowed by a second rule that never injects anything.
    pub plain_allowed_hosts: Vec<String>,
    /// Network services, built by the test against its fake upstreams.
    pub interceptors: Vec<Rc<dyn Interceptor>>,
    /// Hosts of enabled services that cannot run (denied).
    pub unavailable_targets: Vec<crate::network::target::NetworkTarget>,
}

impl Default for TestNetworkConfig {
    fn default() -> Self {
        Self {
            allowed_hosts: vec!["*".into()],
            middleware_scripts: vec![],
            trust_cas: vec![],
            inject: vec![],
            plain_allowed_hosts: vec![],
            interceptors: vec![],
            unavailable_targets: vec![],
        }
    }
}

/// Full test runner: provides proxy, request log, and the MITM CA PEM
/// (for container-side TLS clients to trust).
pub fn run_with_config<F, Fut>(cfg: TestNetworkConfig, f: F)
where
    F: FnOnce(network_proxy::Client, RequestLog, String /* mitm_ca_pem */) -> Fut,
    Fut: Future<Output = ()>,
{
    let (log, mitm_ca_pem, network) = build_network(cfg);
    run_network(network, |proxy| f(proxy, log, mitm_ca_pem));
}

/// Serve `network` over in-memory RPC and run `f` with the proxy client.
pub fn run_network<F, Fut>(network: Network, f: F)
where
    F: FnOnce(network_proxy::Client) -> Fut,
    Fut: Future<Output = ()>,
{
    block_on_local(async move { f(start_rpc(network)).await });
}

/// Test runner that also subscribes to the monitor event stream, before
/// the proxy starts so no event is missed.
pub fn run_with_events<F, Fut>(cfg: TestNetworkConfig, f: F)
where
    F: FnOnce(network_proxy::Client, broadcast::Receiver<airlock_monitor::NetworkEvent>) -> Fut,
    Fut: Future<Output = ()>,
{
    block_on_local(async move {
        let (_log, _mitm_ca_pem, network) = build_network(cfg);
        let events = network.handle().events();
        let proxy = start_rpc(network);
        f(proxy, events).await;
    });
}

pub fn build_network(cfg: TestNetworkConfig) -> (RequestLog, String, Network) {
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
                inject: cfg.inject.iter().map(|s| s.name.clone()).collect(),
            },
        );
    }

    // Secondary allow rule that never injects — for asserting that the
    // surrogate passes through untouched on hosts the inject rule misses.
    if !cfg.plain_allowed_hosts.is_empty() {
        rules.insert(
            "test-allow-plain".to_string(),
            NetworkRule {
                enabled: true,
                allow: cfg.plain_allowed_hosts,
                deny: vec![],
                passthrough: false,
                inject: vec![],
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
    let config = config_values::Network {
        policy: Policy::DenyByDefault,
        rules,
        middleware: middleware_config,
        ports: BTreeMap::default(),
        sockets: BTreeMap::default(),
        services: BTreeMap::default(),
    };
    let (request_log, log_fn) = RequestLog::new();
    let rule_targets = rules::resolve(&config).unwrap();
    // Tests don't need real secret storage — a disabled vault gives
    // the substitution machinery a no-op backend and never prompts.
    let vault = crate::vault::Vault::for_storage_type(crate::vault::VaultStorageType::Disabled);
    let middleware_targets = rules::resolve_middleware(&config, &vault, &log_fn).unwrap();
    let sandbox_env = SandboxEnv::from_secrets(cfg.inject);
    let inject_targets = rules::resolve_inject(&config, &sandbox_env).unwrap();

    // MITM CA
    let mitm_ca = TestCa::generate();
    let mitm_ca_pem = mitm_ca.cert_pem;
    let interceptor = TlsInterceptor::new(&mitm_ca_pem, &mitm_ca.key_pem).unwrap();

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
            inject_targets,
            interceptors: cfg.interceptors,
            public_only: false,
            unavailable_targets: cfg.unavailable_targets,
            port_forwards: std::collections::HashMap::default(),
            socket_map: std::collections::HashMap::default(),
            // Room for every event a `run_with_events` test reads after
            // the fact. Without a subscriber nothing is sent at all.
            events: tokio::sync::broadcast::channel(64).0,
            next_id: std::sync::atomic::AtomicU64::new(0),
            deny_reporter: crate::network::DenyReporter::new(),
        },
    )
}

pub fn start_rpc(network: Network) -> network_proxy::Client {
    rpc_loopback(capnp_rpc::new_client::<network_proxy::Client, _>(network).client)
}

/// A network built by [`Network::new`] from real config text, with the
/// sandbox env resolved from its `[env]` section.
pub struct ConfigNetwork {
    pub network: Network,
    pub env: SandboxEnv,
}

/// Build the network of a project whose `airlock.toml` is `toml`, the way
/// `airlock start` does: config resolution, `[env]` resolution, then
/// [`Network::new`] with its validation.
pub fn network_from_toml(toml: &str) -> anyhow::Result<ConfigNetwork> {
    let config = super::resolve_project_toml(toml)?.values;
    let home = temp_dir();
    let context = super::test_context(
        home.path(),
        crate::vault::Vault::for_storage_type(crate::vault::VaultStorageType::Disabled),
    );
    let env = crate::project::resolve_env(&config, &context.vault)?;
    let ca = TestCa::generate();
    let project = Project {
        sandbox_dir: home.path().join("sandbox"),
        host_home: home.path().to_path_buf(),
        host_cwd: home.path().to_path_buf(),
        guest_cwd: home.path().to_path_buf(),
        config,
        env: env.clone(),
        ca_cert: ca.cert_pem,
        ca_key: ca.key_pem,
        context,
    };
    let network = Network::new(
        &project,
        "/root",
        crate::network::native_tls_client(),
        vec![],
        vec![],
    )?;
    Ok(ConfigNetwork { network, env })
}

/// A TCP listener on `127.0.0.1` that counts the connections it accepts
/// and answers each with an empty `204`.
pub struct AcceptCounter {
    listener: StdListener,
    accepted: Arc<AtomicUsize>,
}

impl AcceptCounter {
    /// Bind now (outside the runtime).
    pub fn bind() -> Self {
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            listener,
            accepted: Arc::default(),
        }
    }

    pub fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }

    /// Start accepting (inside the runtime). The returned counter counts
    /// every accepted connection.
    pub fn start(self) -> Arc<AtomicUsize> {
        let listener = tokio::net::TcpListener::from_std(self.listener).unwrap();
        let accepted = self.accepted.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = [0u8; 1024];
                    let _ = sock.read(&mut buf).await;
                    let _ = sock
                        .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                        .await;
                });
            }
        });
        self.accepted
    }
}

// ── Test connection ─────────────────────────────────────

pub struct TestConnection {
    server_sink: tcp_sink::Client,
    pub container_rx: mpsc::Receiver<Bytes>,
}

impl TestConnection {
    pub async fn connect(proxy: &network_proxy::Client, host: &str, port: u16) -> Option<Self> {
        let (tx, container_rx) = mpsc::channel::<Bytes>(16);
        let client_sink: tcp_sink::Client = capnp_rpc::new_client(CollectorSink::new(tx));

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

    /// Connect to `127.0.0.1:port`, which the network must accept.
    pub async fn local(proxy: &network_proxy::Client, port: u16) -> Self {
        Self::connect(proxy, "127.0.0.1", port)
            .await
            .expect("the proxy accepts the connection")
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

/// The guest's TLS session to `127.0.0.1:port` through the proxy,
/// trusting the sandbox CA `mitm_ca` and offering `alpn`.
pub async fn guest_tls(
    proxy: &network_proxy::Client,
    mitm_ca: &str,
    port: u16,
    alpn: &[&[u8]],
) -> tokio_rustls::client::TlsStream<RpcStream> {
    let conn = TestConnection::local(proxy, port).await;
    let mut config = tls_trusting(mitm_ca);
    config.alpn_protocols = alpn.iter().map(|p| p.to_vec()).collect();
    tokio_rustls::TlsConnector::from(Arc::new(config))
        .connect(
            rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap(),
            conn.into_stream(),
        )
        .await
        .unwrap()
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

struct CollectorSink(std::cell::RefCell<Option<mpsc::Sender<Bytes>>>);

impl CollectorSink {
    fn new(tx: mpsc::Sender<Bytes>) -> Self {
        Self(std::cell::RefCell::new(Some(tx)))
    }
}

impl tcp_sink::Server for CollectorSink {
    async fn send(self: Rc<Self>, params: tcp_sink::SendParams) -> Result<(), capnp::Error> {
        let data = params.get()?.get_data()?;
        let tx = self.0.borrow().clone();
        if let Some(tx) = tx {
            let _ = tx.send(Bytes::copy_from_slice(data)).await;
        }
        Ok(())
    }

    /// Drop the sender so the container side reads EOF, like a real FIN.
    async fn close(
        self: Rc<Self>,
        _params: tcp_sink::CloseParams,
        _results: tcp_sink::CloseResults,
    ) -> Result<(), capnp::Error> {
        self.0.borrow_mut().take();
        Ok(())
    }
}

// ── Request builders ────────────────────────────────────

pub fn http_get(port: u16, path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
}

pub fn http_get_keepalive(port: u16, path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
}

pub fn http_post(port: u16, path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
