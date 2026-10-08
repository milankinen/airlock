//! The network harness: a sandbox network that a small test config builds,
//! an in-memory Cap'n Proto RPC server for it, and a guest-side connection
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

/// Keeps the messages of Lua `log()` calls, for test checks.
#[derive(Clone)]
pub struct RequestLog(Rc<std::cell::RefCell<Vec<String>>>);

impl RequestLog {
    /// Make an empty log.
    /// Returns:
    ///   The log and the log function to give to the middleware.
    pub fn new() -> (Self, LogFn) {
        let log = Self(Rc::new(std::cell::RefCell::new(Vec::new())));
        let inner = log.0.clone();
        let log_fn: LogFn = Rc::new(move |msg: &str| inner.borrow_mut().push(msg.to_string()));
        (log, log_fn)
    }

    /// All messages so far, in order.
    pub fn messages(&self) -> Vec<String> {
        self.0.borrow().clone()
    }
}

// ── Network + RPC harness ───────────────────────────────

/// The config of a test network. The default allows all hosts.
pub struct TestNetworkConfig {
    /// Hosts that the `test-allow` rule allows. Middleware applies to them.
    pub allowed_hosts: Vec<String>,
    /// Middleware as `(name, Lua script)`. The name is not used.
    pub middleware_scripts: Vec<(&'static str, &'static str)>,
    /// More CA PEMs to trust, for example the CAs of test servers.
    pub trust_cas: Vec<String>,
    /// Masked secrets that the `test-allow` rule injects on all its hosts.
    pub inject: Vec<MaskedSecret>,
    /// Hosts that a second rule allows. This rule injects nothing.
    pub plain_allowed_hosts: Vec<String>,
    /// Network services that the test builds over its fake upstreams.
    pub interceptors: Vec<Rc<dyn Interceptor>>,
    /// Hosts of enabled services that cannot run. The network denies them.
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

/// Build a network from `cfg`, serve it, and run `f` with the proxy client,
/// the request log and the MITM CA PEM. Guest-side TLS clients trust this
/// CA.
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

/// Build a network from `cfg`, serve it, and run `f` with the proxy client
/// and the monitor events. The subscription starts before the proxy, so
/// that no event is lost.
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

/// Build a deny-by-default network from `cfg`, with a new MITM CA.
/// Returns:
///   The request log, the MITM CA PEM and the network.
pub fn build_network(cfg: TestNetworkConfig) -> (RequestLog, String, Network) {
    // Make the rules from the test config. Rules only allow or deny. The
    // middleware comes later.
    let mut rules = BTreeMap::new();
    let middleware_targets = cfg.allowed_hosts.clone();

    // The main allow rule.
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

    // A second allow rule that never injects. Tests use it to check that
    // the surrogate stays unchanged on hosts that the inject rule does not
    // cover.
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

    // Make the middleware from the test config. It applies to the allowed
    // hosts. MITM is always on for allowed hosts, also with no middleware,
    // so the harness does not need an empty middleware.
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

    // Deny by default: the network allows only the listed hosts.
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
    // Tests need no real secret storage. A disabled vault gives the
    // substitution an empty backend and never asks the user.
    let vault = crate::vault::Vault::for_storage_type(crate::vault::VaultStorageType::Disabled);
    let middleware_targets = rules::resolve_middleware(&config, &vault, &log_fn).unwrap();
    let sandbox_env = SandboxEnv::from_secrets(cfg.inject);
    let inject_targets = rules::resolve_inject(&config, &sandbox_env).unwrap();

    // The MITM CA of the sandbox.
    let mitm_ca = TestCa::generate();
    let mitm_ca_pem = mitm_ca.cert_pem;
    let interceptor = TlsInterceptor::new(&mitm_ca_pem, &mitm_ca.key_pem).unwrap();

    // The upstream TLS client trusts the system roots and the test CAs.
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
            // Room for all events that a `run_with_events` test reads
            // later. With no subscriber, the network sends no events.
            events: tokio::sync::broadcast::channel(64).0,
            next_id: std::sync::atomic::AtomicU64::new(0),
            deny_reporter: crate::network::DenyReporter::new(),
        },
    )
}

/// Serve `network` over in-memory RPC.
/// Returns:
///   The proxy client of the guest.
pub fn start_rpc(network: Network) -> network_proxy::Client {
    rpc_loopback(capnp_rpc::new_client::<network_proxy::Client, _>(network).client)
}

/// A network that [`Network::new`] builds from real config text, and the
/// sandbox env from its `[env]` section.
pub struct ConfigNetwork {
    /// The network of the project.
    pub network: Network,
    /// The sandbox env from `[env]`.
    pub env: SandboxEnv,
}

/// Build the network of a project whose `airlock.toml` is `toml`, as
/// `airlock start` does: resolve the config, resolve `[env]`, then call
/// [`Network::new`] with its checks.
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
    /// Bind the listener now, outside the runtime.
    pub fn bind() -> Self {
        let listener = StdListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        Self {
            listener,
            accepted: Arc::default(),
        }
    }

    /// The port of the listener.
    pub fn port(&self) -> u16 {
        self.listener.local_addr().unwrap().port()
    }

    /// Start to accept connections, inside the runtime.
    /// Returns:
    ///   The number of accepted connections, which increases over time.
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

/// A guest-side TCP connection through the proxy. The data from the
/// server arrives in `container_rx`.
pub struct TestConnection {
    server_sink: tcp_sink::Client,
    /// The data chunks from the server, in order.
    pub container_rx: mpsc::Receiver<Bytes>,
}

impl TestConnection {
    /// Connect to `host:port` through the proxy.
    /// Returns:
    ///   The connection, or `None` if the network denies it. Panics on a
    ///   connect error.
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

    /// Send `data` to the server.
    pub async fn send(&self, data: &[u8]) {
        let mut req = self.server_sink.send_request();
        req.get().set_data(data);
        req.send().await.unwrap();
    }

    /// Send `request` and read the answer for up to 3 seconds.
    pub async fn roundtrip(&mut self, request: &str) -> String {
        self.send(request.as_bytes()).await;
        self.recv(3000).await
    }

    /// Read data until the server closes the connection or `timeout_ms`
    /// passes.
    /// Returns:
    ///   All data read, as lossy UTF-8.
    pub async fn recv(&mut self, timeout_ms: u64) -> String {
        self.recv_and_close(timeout_ms).await.0
    }

    /// Read data until the server closes the connection. Panics if it does
    /// not close in `timeout_ms`.
    /// Returns:
    ///   All data read, as lossy UTF-8.
    pub async fn recv_until_closed(&mut self, timeout_ms: u64) -> String {
        let (data, closed) = self.recv_and_close(timeout_ms).await;
        assert!(
            closed,
            "connection still open after {timeout_ms} ms: {data}"
        );
        data
    }

    /// Read data until the server closes the connection or `timeout_ms`
    /// passes.
    /// Returns:
    ///   All data read, as lossy UTF-8, and true if the server closed the
    ///   connection.
    async fn recv_and_close(&mut self, timeout_ms: u64) -> (String, bool) {
        let mut buf = bytes::BytesMut::new();
        let deadline = tokio::time::Instant::now() + tokio::time::Duration::from_millis(timeout_ms);
        let closed = loop {
            tokio::select! {
                data = self.container_rx.recv() => {
                    match data {
                        Some(chunk) => buf.extend_from_slice(&chunk),
                        None => break true,
                    }
                }
                () = tokio::time::sleep_until(deadline) => break false,
            }
        };
        (String::from_utf8_lossy(&buf).into_owned(), closed)
    }

    /// Change this connection into an `AsyncRead` and `AsyncWrite` stream.
    /// TLS tests use it to do a guest TLS handshake through the RPC
    /// channel.
    pub fn into_stream(self) -> RpcStream {
        RpcStream {
            tx: self.server_sink,
            rx: self.container_rx,
            pending: Bytes::new(),
        }
    }
}

/// Open a guest TLS session to `127.0.0.1:port` through the proxy. The
/// session trusts the sandbox CA `mitm_ca` and offers the protocols
/// `alpn`.
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

/// An `AsyncRead` and `AsyncWrite` stream over the RPC channel, on the
/// guest side. With it, the test guest can do TLS through the proxy.
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

/// A TCP sink that sends each received chunk to a channel.
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

    /// Drop the sender, so the guest side reads EOF as after a real FIN.
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

/// An HTTP/1.1 GET request for `path` that closes the connection.
pub fn http_get(port: u16, path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n")
}

/// An HTTP/1.1 GET request for `path` that keeps the connection open.
pub fn http_get_keepalive(port: u16, path: &str) -> String {
    format!("GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n\r\n")
}

/// An HTTP/1.1 POST request for `path` with `body`, that closes the
/// connection.
pub fn http_post(port: u16, path: &str, body: &str) -> String {
    format!(
        "POST {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    )
}
