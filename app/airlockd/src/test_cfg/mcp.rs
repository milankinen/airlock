//! Fake host MCP server and HTTP client helpers for tests of the MCP
//! endpoints.

use std::cell::{Cell, RefCell};
use std::fmt::Write as _;
use std::net::SocketAddr;
use std::rc::Rc;

use airlock_common::supervisor_capnp::{mcp_grant, mcp_host, mcp_session, mcp_sink};
use airlock_test_utils::{read_until_contains, read_until_eof, rpc_loopback, serve};
use serde_json::{Value, json};
use tokio::io::AsyncWriteExt;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, oneshot};

use crate::admin::AdminState;
use crate::admin::server::app;
use crate::mcp::{self, Routes};

/// Name of the fake MCP server.
pub(crate) const ECHO: &str = "echo";

/// A granted server name that the fake host refuses to start.
pub(crate) const BROKEN: &str = "broken";

/// A fake host that starts a tiny echo MCP server for each session. The
/// guest uses it through a real RPC connection. Call
/// [`FakeMcpHost::client`] in a `LocalSet`.
///
/// The server answers `initialize`, `tools/list` and `tools/call`. Before
/// the `tools/call` result, it sends a progress notification and a
/// `roots/list` server request. On the `test/emit` notification, it sends
/// an unsolicited log notification. On the `test/exit` request, the
/// process stops with no answer. Other requests get no answer.
#[derive(Default)]
pub(crate) struct FakeMcpHost {
    /// Messages that the server processes got, in order.
    pub received: RefCell<Vec<Value>>,
    /// Number of server processes that run now.
    pub running: Cell<usize>,
    /// Number of `open` calls that started.
    pub opens: Cell<usize>,
    /// Number of server processes that started.
    pub started: Cell<usize>,
    /// If set, the next `open` waits for this signal before it returns.
    pub open_gate: RefCell<Option<oneshot::Receiver<()>>>,
}

impl FakeMcpHost {
    /// Return an RPC client for this host.
    pub fn client(self: &Rc<Self>) -> mcp_host::Client {
        let server: mcp_host::Client = capnp_rpc::new_client(HostServer(self.clone()));
        rpc_loopback(server.client)
    }

    /// Return the received messages that have `method`.
    pub fn received_with(&self, method: &str) -> Vec<Value> {
        self.received
            .borrow()
            .iter()
            .filter(|m| m["method"] == method)
            .cloned()
            .collect()
    }
}

/// RPC server of [`FakeMcpHost`].
struct HostServer(Rc<FakeMcpHost>);

impl mcp_host::Server for HostServer {
    async fn open(
        self: Rc<Self>,
        params: mcp_host::OpenParams,
        mut results: mcp_host::OpenResults,
    ) -> Result<(), capnp::Error> {
        self.0.opens.set(self.0.opens.get() + 1);
        let gate = self.0.open_gate.borrow_mut().take();
        if let Some(gate) = gate {
            let _ = gate.await;
        }
        let params = params.get()?;
        if params.get_name()?.to_str()? != ECHO {
            return Err(capnp::Error::failed("unknown MCP server".into()));
        }
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::task::spawn_local(echo_server(rx, params.get_sink()?));
        self.0.running.set(self.0.running.get() + 1);
        self.0.started.set(self.0.started.get() + 1);
        results
            .get()
            .set_session(capnp_rpc::new_client(FakeProcess {
                host: self.0.clone(),
                stdin: tx,
            }));
        Ok(())
    }
}

/// One fake server process. The guest releases it to stop the process.
struct FakeProcess {
    host: Rc<FakeMcpHost>,
    stdin: mpsc::UnboundedSender<Value>,
}

impl Drop for FakeProcess {
    fn drop(&mut self) {
        self.host.running.set(self.host.running.get() - 1);
    }
}

impl mcp_session::Server for FakeProcess {
    async fn send(
        self: Rc<Self>,
        params: mcp_session::SendParams,
        _results: mcp_session::SendResults,
    ) -> Result<(), capnp::Error> {
        let line = params.get()?.get_message()?.to_str()?;
        assert!(!line.contains('\n'), "message is not one line: {line}");
        let message: Value = serde_json::from_str(line).unwrap();
        self.host.received.borrow_mut().push(message.clone());
        let _ = self.stdin.send(message);
        Ok(())
    }
}

/// Answer the messages of one fake server process, one at a time, as a
/// stdio server does.
async fn echo_server(mut stdin: mpsc::UnboundedReceiver<Value>, sink: mcp_sink::Client) {
    while let Some(message) = stdin.recv().await {
        let id = &message["id"];
        let result = |result: Value| json!({ "jsonrpc": "2.0", "id": id, "result": result });
        let out = match message["method"].as_str() {
            Some("initialize") => vec![result(json!({
                "protocolVersion": "2025-06-18",
                "capabilities": { "tools": {} },
                "serverInfo": { "name": ECHO, "version": "1" },
            }))],
            Some("tools/list") => vec![result(json!({ "tools": [{ "name": "echo" }] }))],
            Some("tools/call") => vec![
                json!({ "jsonrpc": "2.0", "method": "notifications/progress",
                        "params": { "progressToken": 1, "progress": 1 } }),
                json!({ "jsonrpc": "2.0", "id": "srv-1", "method": "roots/list" }),
                result(json!({ "content": [{ "type": "text",
                                             "text": message["params"]["arguments"]["text"] }] })),
            ],
            Some("test/emit") => vec![json!({ "jsonrpc": "2.0", "method": "notifications/message",
                                              "params": { "level": "info", "data": "unsolicited" } })],
            Some("test/exit") => {
                let mut req = sink.closed_request();
                req.get().set_reason("exit code 0");
                let _ = req.send().promise.await;
                return;
            }
            _ => vec![],
        };
        for m in out {
            let mut req = sink.message_request();
            req.get().set_message(m.to_string());
            req.send().promise.await.unwrap();
        }
    }
}

/// Serve all routes of the local server on a local port. The boot config
/// MCP routes use `host`, which grants the servers [`ECHO`] and [`BROKEN`].
/// `None` grants no MCP servers.
pub(crate) async fn serve_local(host: Option<&Rc<FakeMcpHost>>) -> SocketAddr {
    serve_mcp(host).await.0
}

/// Serve all routes of the local server on a local port, as
/// [`serve_local`] does.
///
/// Returns:
///   The address and the route handle that adds run-time MCP servers.
pub(crate) async fn serve_mcp(host: Option<&Rc<FakeMcpHost>>) -> (SocketAddr, Routes) {
    let (bridge, routes) = mcp::start();
    if let Some(host) = host {
        routes.add_boot(&host.client(), vec![ECHO.to_string(), BROKEN.to_string()]);
    }
    (serve(app(AdminState::new(), bridge)).await, routes)
}

/// Add a run-time MCP server of `host` at
/// `http://acp.mcp.airlock/<session>/<name>`.
///
/// Returns:
///   The grant through a real RPC connection, as the host gets it.
pub(crate) async fn add_runtime(
    routes: &Routes,
    session: &str,
    name: &str,
    host: &Rc<FakeMcpHost>,
) -> Result<mcp_grant::Client, capnp::Error> {
    let grant = mcp::add_server(routes, session, name, host.client()).await?;
    Ok(rpc_loopback(grant.client))
}

/// One HTTP response, with the chunked body decoded.
#[derive(Debug)]
pub(crate) struct HttpReply {
    /// Status code.
    pub status: u16,
    /// Status line and header lines.
    pub head: String,
    /// Body text.
    pub body: String,
}

impl HttpReply {
    /// Return the value of header `name`.
    pub fn header(&self, name: &str) -> Option<String> {
        self.head.lines().find_map(|line| {
            let (n, v) = line.split_once(':')?;
            n.eq_ignore_ascii_case(name).then(|| v.trim().to_string())
        })
    }

    /// Return the JSON data of each event of an event stream body.
    pub fn events(&self) -> Vec<Value> {
        sse_events(&self.body)
    }
}

/// Return the JSON data of each `data:` line in an event stream text.
pub(crate) fn sse_events(text: &str) -> Vec<Value> {
    text.lines()
        .filter_map(|l| l.strip_prefix("data: "))
        .map(|d| serde_json::from_str(d).unwrap())
        .collect()
}

/// Send one HTTP/1.1 request and read the full response.
/// Args:
///  - `headers`: Header lines, for example `"Host: mcp.airlock"`
///  - `body`: JSON body for POST
pub(crate) async fn http(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[&str],
    body: Option<&Value>,
) -> HttpReply {
    let mut stream = send_request(addr, method, path, headers, body).await;
    let response = read_until_eof(&mut stream).await;
    let (head, body) = response.split_once("\r\n\r\n").unwrap();
    let status = head[9..12].parse().unwrap();
    let mut reply = HttpReply {
        status,
        head: head.to_string(),
        body: body.to_string(),
    };
    if reply
        .header("transfer-encoding")
        .is_some_and(|v| v == "chunked")
    {
        reply.body = dechunk(body);
    }
    reply
}

/// Open a connection and send one HTTP/1.1 request on it.
async fn send_request(
    addr: SocketAddr,
    method: &str,
    path: &str,
    headers: &[&str],
    body: Option<&Value>,
) -> TcpStream {
    let body = body.map(Value::to_string).unwrap_or_default();
    let mut request = format!("{method} {path} HTTP/1.1\r\nConnection: close\r\n");
    for h in headers {
        let _ = write!(request, "{h}\r\n");
    }
    let _ = write!(request, "Content-Length: {}\r\n\r\n{body}", body.len());
    let mut stream = TcpStream::connect(addr).await.unwrap();
    stream.write_all(request.as_bytes()).await.unwrap();
    stream
}

/// Decode a chunked HTTP body.
fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    while let Some((size, rest)) = body.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap();
        if size == 0 {
            break;
        }
        out.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
    out
}

/// MCP client of one session on one server.
pub(crate) struct McpClient {
    addr: SocketAddr,
    /// Host name of the server URL.
    host: &'static str,
    /// Path of the server URL.
    path: String,
    /// Session ID from the `initialize` response.
    pub session: Option<String>,
}

impl McpClient {
    /// Make a client of the boot config server [`ECHO`] with no session.
    pub fn new(addr: SocketAddr) -> Self {
        Self::at(addr, mcp::CONFIG_HOST, "/echo")
    }

    /// Make a client of the server at `http://<host><path>` with no
    /// session.
    pub fn at(addr: SocketAddr, host: &'static str, path: &str) -> Self {
        Self {
            addr,
            host,
            path: path.to_string(),
            session: None,
        }
    }

    /// Return the request headers: host, accepted types and session.
    fn headers(&self, accept: &'static str) -> Vec<String> {
        let mut headers = vec![format!("Host: {}", self.host), format!("Accept: {accept}")];
        if let Some(sid) = &self.session {
            headers.push(format!("Mcp-Session-Id: {sid}"));
        }
        headers
    }

    /// Send `initialize` and keep the session ID of the response.
    pub async fn initialize(&mut self) -> HttpReply {
        let reply = self
            .post(&json!({ "jsonrpc": "2.0", "id": 0, "method": "initialize",
                           "params": { "protocolVersion": "2025-06-18" } }))
            .await;
        self.session = reply.header("mcp-session-id");
        reply
    }

    /// POST one JSON-RPC message.
    pub async fn post(&self, message: &Value) -> HttpReply {
        let headers = self.headers("application/json, text/event-stream");
        let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
        http(self.addr, "POST", &self.path, &headers, Some(message)).await
    }

    /// POST one JSON-RPC message and do not read the response.
    ///
    /// Returns:
    ///   The open connection.
    pub async fn post_open(&self, message: &Value) -> TcpStream {
        let headers = self.headers("application/json, text/event-stream");
        let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
        send_request(self.addr, "POST", &self.path, &headers, Some(message)).await
    }

    /// Open the standalone event stream and wait for the response head.
    ///
    /// Returns:
    ///   The open connection and the text read so far.
    pub async fn listen(&self) -> (TcpStream, String) {
        let headers = self.headers("text/event-stream");
        let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
        let mut stream = send_request(self.addr, "GET", &self.path, &headers, None).await;
        let head = read_until_contains(&mut stream, "\r\n\r\n").await;
        (stream, head)
    }

    /// GET the standalone event stream when the result is an error.
    pub async fn listen_refused(&self) -> HttpReply {
        let headers = self.headers("text/event-stream");
        let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
        http(self.addr, "GET", &self.path, &headers, None).await
    }

    /// End the session with DELETE.
    pub async fn delete(&self) -> HttpReply {
        let headers = self.headers("*/*");
        let headers: Vec<&str> = headers.iter().map(String::as_str).collect();
        http(self.addr, "DELETE", &self.path, &headers, None).await
    }
}
