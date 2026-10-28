//! Fake remote MCP servers over HTTP, and a host that serves one remote
//! server to the guest.

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use airlock_common::supervisor_capnp::mcp_host;
use axum::Router;
use axum::body::Body;
use axum::extract::State;
use axum::http::{HeaderMap, Method, StatusCode, Uri};
use axum::response::Response;
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::oci::verified_dir;
use super::{rpc_loopback, serve};
use crate::mcp::{HostStopper, McpHostImpl, RemoteKind, RemoteSpec, ServerSpec, Transport};

/// Name of the remote server in the host.
pub const REMOTE_NAME: &str = "remote";

/// Session ID that the fake streamable server gives.
pub const SESSION_ID: &str = "sess-1";

/// Protocol version that the fake servers give.
pub const PROTOCOL_VERSION: &str = "2025-06-18";

/// Headers of the owner that the tests give to the relay.
pub const OWNER_HEADERS: &[(&str, &str)] =
    &[("authorization", "Bearer secret"), ("x-editor", "zed")];

/// One HTTP request that a fake server got.
#[derive(Debug, Clone)]
pub struct Recorded {
    /// HTTP method.
    pub method: Method,
    /// Path and query.
    pub target: String,
    /// Request headers.
    pub headers: HeaderMap,
    /// Request body as text.
    pub body: String,
}

impl Recorded {
    /// Value of the header `name`, if it is there.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers.get(name).and_then(|v| v.to_str().ok())
    }

    /// JSON-RPC method of the body, if any.
    pub fn rpc_method(&self) -> Option<String> {
        let body: Value = serde_json::from_str(&self.body).ok()?;
        body.get("method")?.as_str().map(str::to_string)
    }
}

/// Fake remote MCP server. Records each request. The tests push messages
/// to its open event stream.
#[derive(Clone, Default)]
pub struct FakeMcp {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    requests: Mutex<Vec<Recorded>>,
    /// Sender of the open event stream.
    stream: Mutex<Option<mpsc::UnboundedSender<String>>>,
    /// Message URL that the legacy server gives in its endpoint event.
    endpoint: Mutex<String>,
}

impl FakeMcp {
    /// All requests so far.
    pub fn requests(&self) -> Vec<Recorded> {
        self.inner.requests.lock().unwrap().clone()
    }

    /// Check if an event stream is open.
    pub fn stream_open(&self) -> bool {
        self.inner.stream.lock().unwrap().is_some()
    }

    /// Send the JSON-RPC message `message` as a `message` event on the open
    /// event stream.
    pub fn push(&self, message: &Value) {
        let stream = self.inner.stream.lock().unwrap();
        let stream = stream.as_ref().expect("open event stream");
        stream
            .send(format!("event: message\ndata: {message}\n\n"))
            .unwrap();
    }

    /// End the open event stream.
    pub fn end_stream(&self) {
        self.inner.stream.lock().unwrap().take();
    }

    /// Open an event stream. It starts with `first` and continues with the
    /// pushed events.
    fn open_stream(&self, first: String) -> Response {
        let (tx, rx) = mpsc::unbounded_channel();
        tx.send(first).unwrap();
        *self.inner.stream.lock().unwrap() = Some(tx);
        let body = futures::stream::unfold(rx, |mut rx| async move {
            let event = rx.recv().await?;
            Some((Ok::<_, Infallible>(event), rx))
        });
        event_stream(Body::from_stream(body))
    }

    fn record(&self, method: Method, uri: &Uri, headers: HeaderMap, body: String) {
        self.inner.requests.lock().unwrap().push(Recorded {
            method,
            target: uri
                .path_and_query()
                .map_or_else(|| uri.path().to_string(), ToString::to_string),
            headers,
            body,
        });
    }
}

/// Response with an event stream body.
fn event_stream(body: Body) -> Response {
    Response::builder()
        .header("content-type", "text/event-stream")
        .body(body)
        .unwrap()
}

/// Response with a JSON body.
fn json_response(value: &Value) -> Response {
    Response::builder()
        .header("content-type", "application/json")
        .body(Body::from(value.to_string()))
        .unwrap()
}

/// Response with only a status.
fn status(code: StatusCode) -> Response {
    Response::builder()
        .status(code)
        .body(Body::empty())
        .unwrap()
}

/// Start a fake Streamable HTTP MCP server at `/mcp`. It answers these
/// JSON-RPC methods:
///  - `initialize`: JSON result with a session ID header
///  - notifications and responses: 202
///  - `tools/list`: JSON result
///  - `tools/call`: event stream with a progress notification, then the
///    result
///  - `fail`: HTTP 500
///  - `expire`: HTTP 404, as for an unknown session
///  - `redirect`: HTTP 307 to `/elsewhere`
///
/// `GET /mcp` with a session ID opens the event stream for pushed
/// messages. `DELETE` gives 200.
/// Returns:
///   The server state and the URL of the MCP endpoint.
pub async fn streamable_server() -> (FakeMcp, String) {
    let fake = FakeMcp::default();
    let app = Router::new()
        .fallback(streamable_handler)
        .with_state(fake.clone());
    let addr = serve(app).await;
    (fake, format!("http://{addr}/mcp"))
}

async fn streamable_handler(
    State(fake): State<FakeMcp>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    let has_session = headers.get("mcp-session-id").is_some();
    fake.record(method.clone(), &uri, headers, body.clone());
    if uri.path() != "/mcp" {
        return status(StatusCode::OK);
    }
    match method {
        Method::GET if has_session => fake.open_stream(": open\n\n".into()),
        Method::GET => status(StatusCode::BAD_REQUEST),
        Method::DELETE => status(StatusCode::OK),
        _ => streamable_post(&body),
    }
}

fn streamable_post(body: &str) -> Response {
    let body: Value = serde_json::from_str(body).unwrap();
    let id = body.get("id").cloned().unwrap_or(Value::Null);
    let result = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    match body.get("method").and_then(Value::as_str) {
        Some("initialize") => {
            let mut response = json_response(&result(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "fake", "version": "1"},
            })));
            response
                .headers_mut()
                .insert("mcp-session-id", SESSION_ID.parse().unwrap());
            response
        }
        None => status(StatusCode::ACCEPTED),
        Some(m) if m.starts_with("notifications/") => status(StatusCode::ACCEPTED),
        Some("tools/list") => json_response(&result(json!({"tools": [{"name": "echo"}]}))),
        Some("tools/call") => {
            let progress = json!({
                "jsonrpc": "2.0",
                "method": "notifications/progress",
                "params": {"progressToken": 1, "progress": 1},
            });
            let done = result(json!({"content": [{"type": "text", "text": "done"}]}));
            // CRLF line ends, a comment and an ID, as real servers send.
            event_stream(Body::from(format!(
                ": start\r\nid: 1\r\nevent: message\r\ndata: {progress}\r\n\r\ndata: {done}\r\n\r\n"
            )))
        }
        Some("fail") => status(StatusCode::INTERNAL_SERVER_ERROR),
        Some("expire") => status(StatusCode::NOT_FOUND),
        Some("redirect") => Response::builder()
            .status(StatusCode::TEMPORARY_REDIRECT)
            .header("location", "/elsewhere")
            .body(Body::empty())
            .unwrap(),
        Some(_) => json_response(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {"code": -32601, "message": "unknown method"},
        })),
    }
}

/// Start a fake legacy SSE MCP server. `GET /sse` opens the event stream
/// and sends the endpoint event first. `POST /messages` gives 202 and
/// sends the answer of each request on the stream: a result with the
/// method name.
/// Args:
///  - `endpoint`: Makes the endpoint event data from the server address
///
/// Returns:
///   The server state and the URL of the event stream.
pub async fn legacy_server(endpoint: impl FnOnce(SocketAddr) -> String) -> (FakeMcp, String) {
    let fake = FakeMcp::default();
    let app = Router::new()
        .fallback(legacy_handler)
        .with_state(fake.clone());
    let addr = serve(app).await;
    *fake.inner.endpoint.lock().unwrap() = endpoint(addr);
    (fake, format!("http://{addr}/sse"))
}

async fn legacy_handler(
    State(fake): State<FakeMcp>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: String,
) -> Response {
    fake.record(method.clone(), &uri, headers, body.clone());
    match (method, uri.path()) {
        (Method::GET, "/sse") => {
            let endpoint = fake.inner.endpoint.lock().unwrap().clone();
            fake.open_stream(format!("event: endpoint\ndata: {endpoint}\n\n"))
        }
        (Method::POST, "/messages") => {
            let body: Value = serde_json::from_str(&body).unwrap();
            if let (Some(id), Some(method)) = (body.get("id"), body.get("method")) {
                fake.push(&json!({"jsonrpc": "2.0", "id": id, "result": {"method": method}}));
            }
            status(StatusCode::ACCEPTED)
        }
        _ => status(StatusCode::NOT_FOUND),
    }
}

/// Serve one remote server to a guest client over RPC.
/// Args:
///  - `kind`: Transport of the server
///  - `url`: URL of the server
///
/// Returns:
///   The host client, and the stopper of the host.
pub fn served_remote(kind: RemoteKind, url: &str) -> (mcp_host::Client, HostStopper) {
    let headers: Vec<(String, String)> = OWNER_HEADERS
        .iter()
        .map(|(n, v)| ((*n).into(), (*v).into()))
        .collect();
    let spec = ServerSpec {
        name: REMOTE_NAME.into(),
        transport: Transport::Remote(RemoteSpec::new(kind, url, &headers).unwrap()),
        timeout: None,
        max_sessions: 16,
    };
    let host = McpHostImpl::new(vec![spec], verified_dir(std::path::Path::new("/")));
    let stopper = host.stopper();
    let client = rpc_loopback(capnp_rpc::new_client::<mcp_host::Client, _>(host).client);
    (client, stopper)
}
