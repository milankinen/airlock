//! Remote MCP servers.
//!
//! Relays the JSON-RPC messages of one guest session to an MCP server at a
//! URL, with the headers that the owner of the server gives. Supports the
//! two HTTP transports of MCP:
//!  * Streamable HTTP: each message is one request, and the server answers
//!    with JSON or with an event stream. A separate event stream carries
//!    the messages that the server starts.
//!  * Legacy SSE: one event stream carries all server messages, and the
//!    messages to the server go to a URL that the stream gives.
//!
//! A failed request gives the guest an error response with the request
//! ID. The session stops when the server ends it or when the guest or the
//! host stops it. At the stop, the host tells the server that the session
//! ended, if the server keeps sessions. Requests do not follow redirects,
//! so the headers never go to another server. Large messages and events
//! are refused. The logs never show header values or full URLs.

mod legacy_sse;
mod sse;
mod streamable;

use std::cell::Cell;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::supervisor_capnp::{mcp_session, mcp_sink};
use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::Value;
use tokio::sync::{Notify, mpsc};
use url::Url;

use super::ServerSpec;
use super::session::{SessionState, send_closed, sink_grace_over};
use crate::messages;

/// Largest message, event stream line and event from the server in bytes.
const MAX_MESSAGE: usize = 16 * 1024 * 1024;

/// Number of guest messages that can wait for the relay. When the queue is
/// full, `send` fails. Thus a slow server cannot block the guest.
const SEND_QUEUE: usize = 64;

/// Number of server messages that can wait for the guest sink.
const SINK_QUEUE: usize = 64;

/// Time to connect to the server.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

/// Header names that the owner cannot give. The relay sets them itself,
/// or they can break the HTTP framing.
const RESERVED_HEADERS: &[&str] = &[
    "accept",
    "content-type",
    "content-length",
    "host",
    "mcp-session-id",
    "mcp-protocol-version",
    "last-event-id",
    // Hop-by-hop headers can break the HTTP framing.
    "connection",
    "keep-alive",
    "proxy-connection",
    "te",
    "transfer-encoding",
    "upgrade",
];

/// HTTP transport of a remote MCP server.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RemoteKind {
    /// Streamable HTTP transport.
    Http,
    /// Legacy transport with one event stream and a message URL.
    Sse,
}

/// Address of a remote MCP server.
#[derive(Clone)]
pub(crate) struct RemoteSpec {
    /// Transport of the server.
    pub(crate) kind: RemoteKind,
    /// URL of the server, `http` or `https`.
    pub(crate) url: Url,
    /// Headers for each request, for example the credentials of the
    /// server.
    pub(crate) headers: Vec<(HeaderName, HeaderValue)>,
}

impl std::fmt::Debug for RemoteSpec {
    // The URL path and the header values can contain secrets.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RemoteSpec")
            .field("kind", &self.kind)
            .field("origin", &origin(&self.url))
            .field(
                "headers",
                &self.headers.iter().map(|(n, _)| n).collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl RemoteSpec {
    /// Check and make the address of a remote server.
    /// Args:
    ///  - `kind`: Transport of the server
    ///  - `url`: URL of the server. Only `http` and `https` are accepted.
    ///  - `headers`: Header names and values for each request. Names that
    ///    the relay sets itself are refused.
    ///
    /// Returns:
    ///   The address, or the problem. The problem text does not contain
    ///   header values.
    pub(crate) fn new(
        kind: RemoteKind,
        url: &str,
        headers: &[(String, String)],
    ) -> Result<Self, String> {
        let url = Url::parse(url).map_err(messages::mcp::remote_bad_url)?;
        if !matches!(url.scheme(), "http" | "https") {
            return Err(messages::mcp::remote_bad_scheme(url.scheme()));
        }
        let headers = headers
            .iter()
            .map(|(name, value)| {
                let name = HeaderName::try_from(name.as_str())
                    .map_err(|_| messages::mcp::remote_bad_header_name(name))?;
                if RESERVED_HEADERS.contains(&name.as_str()) {
                    return Err(messages::mcp::remote_reserved_header(&name));
                }
                let mut value = HeaderValue::try_from(value.as_str())
                    .map_err(|_| messages::mcp::remote_bad_header_value(&name))?;
                // Debug output of the HTTP stack then hides the value.
                value.set_sensitive(true);
                Ok((name, value))
            })
            .collect::<Result<_, _>>()?;
        Ok(Self { kind, url, headers })
    }

    /// Headers of the owner as a map.
    fn header_map(&self) -> HeaderMap {
        let mut map = HeaderMap::new();
        for (name, value) in &self.headers {
            map.append(name.clone(), value.clone());
        }
        map
    }
}

/// Origin of `url`, for the logs. The path and the query can contain
/// secrets.
fn origin(url: &Url) -> String {
    url.origin().ascii_serialization()
}

thread_local! {
    /// HTTP client of the relay. All MCP work of the process runs on one
    /// thread. Thus this is one client for the process. Its connections
    /// belong to the runtime that opened them, so each thread must use
    /// only one runtime for the relay.
    static CLIENT: reqwest::Client = reqwest::Client::builder()
        .user_agent(concat!("airlock/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(CONNECT_TIMEOUT)
        // A redirect would send the headers of the owner to another
        // server.
        .redirect(reqwest::redirect::Policy::none())
        // Without this, the client reads the proxy env variables, also
        // without the system proxy feature. The relay connects directly,
        // like the stdio servers do.
        .no_proxy()
        .http1_only()
        .build()
        .expect("HTTP client of the MCP relay");

    /// DELETE requests that did not end yet.
    static DELETES: Rc<Pending> = Rc::default();
}

/// Get the HTTP client of the relay.
fn client() -> reqwest::Client {
    CLIENT.with(Clone::clone)
}

/// Count of running background requests and a signal when the count
/// goes to zero.
#[derive(Default)]
struct Pending {
    count: Cell<usize>,
    idle: Notify,
}

/// Wait until the requests that end the remote sessions on their servers
/// are done. Each such request has a time limit.
pub(crate) async fn deletes_done() {
    let pending = DELETES.with(Clone::clone);
    while pending.count.get() > 0 {
        let idle = pending.idle.notified();
        if pending.count.get() == 0 {
            break;
        }
        idle.await;
    }
}

/// Run `request` in the background and count it in [`deletes_done`].
fn spawn_delete(request: impl Future<Output = ()> + 'static) {
    let counted = Counted::new(DELETES.with(Clone::clone));
    tokio::task::spawn_local(async move {
        let _counted = counted;
        request.await;
    });
}

/// One item of a [`Pending`] count. Drop removes it, also when the
/// runtime drops the task before its end.
struct Counted(Rc<Pending>);

impl Counted {
    /// Add one item to `pending`.
    fn new(pending: Rc<Pending>) -> Self {
        pending.count.set(pending.count.get() + 1);
        Self(pending)
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        let count = self.0.count.get() - 1;
        self.0.count.set(count);
        if count == 0 {
            self.0.idle.notify_waiters();
        }
    }
}

/// Remote server that is ready for a session. The legacy transport has
/// its event stream open here.
pub(super) enum Connected {
    /// Streamable HTTP server. It needs no connection before the first
    /// message.
    Streamable(RemoteSpec),
    /// Legacy server with its open event stream.
    Legacy(Box<legacy_sse::Stream>),
}

/// Prepare a session of the remote server `remote`. For the legacy
/// transport, open the event stream and wait for the message URL.
/// Args:
///  - `name`: Name of the server, for the logs and errors
///  - `remote`: Address of the server
///
/// Returns:
///   The server, ready for [`start`]. Error if the legacy stream does not
///   open or gives no valid message URL.
pub(super) async fn connect(name: &str, remote: &RemoteSpec) -> Result<Connected, capnp::Error> {
    match remote.kind {
        RemoteKind::Http => Ok(Connected::Streamable(remote.clone())),
        RemoteKind::Sse => legacy_sse::connect(name, remote)
            .await
            .map(|stream| Connected::Legacy(Box::new(stream)))
            .map_err(|e| {
                tracing::debug!("mcp {name}: {e}");
                capnp::Error::failed(messages::mcp::remote_connect_failed(name))
            }),
    }
}

/// Cap'n Proto `McpSession` server of one remote session. Drop stops the
/// session.
pub(super) struct RemoteSession {
    /// Queue of the relay task. Each item is one message.
    queue: mpsc::Sender<String>,
    /// Activity and stop signal, shared with the tasks and the host.
    state: Rc<SessionState>,
}

impl RemoteSession {
    /// Shared state of the session, for the session limit of the host.
    pub(super) fn state(&self) -> Rc<SessionState> {
        self.state.clone()
    }
}

/// Start the relay tasks of a session. Call inside a `LocalSet`.
/// Args:
///  - `spec`: Server of the session, for its name and idle timeout
///  - `server`: The server from [`connect`]
///  - `sink`: Guest receiver of the server messages
///  - `on_end`: Called one time after the `closed` call to the guest
///
/// Returns:
///   The session.
pub(super) fn start(
    spec: &ServerSpec,
    server: Connected,
    sink: mcp_sink::Client,
    on_end: impl FnOnce() + 'static,
) -> RemoteSession {
    let state = Rc::new(SessionState::new());
    let (queue, messages) = mpsc::channel(SEND_QUEUE);
    let (out, out_rx) = mpsc::channel(SINK_QUEUE);
    let relay = Relay {
        name: spec.name.clone(),
        out,
        state: state.clone(),
    };
    let on_stop: Box<dyn FnOnce()> = match server {
        Connected::Streamable(remote) => streamable::start(&remote, relay, messages),
        Connected::Legacy(stream) => {
            legacy_sse::start(*stream, &relay, messages);
            Box::new(|| {})
        }
    };
    // Run the stop work at the stop signal, also when the guest does not
    // take the last messages. The writer runs it if this task did not run
    // yet, so that it comes before `closed`.
    let on_stop = Rc::new(Cell::new(Some(on_stop)));
    let run_on_stop = |on_stop: &Cell<Option<Box<dyn FnOnce()>>>| {
        if let Some(f) = on_stop.take() {
            f();
        }
    };
    tokio::task::spawn_local({
        let stop = state.stop_token();
        let on_stop = on_stop.clone();
        async move {
            stop.cancelled().await;
            run_on_stop(&on_stop);
        }
    });
    tokio::task::spawn_local({
        let state = state.clone();
        async move {
            write_sink(&sink, out_rx, &state).await;
            // The writer is the only task that ends the session. Thus the
            // guest gets `closed` one time, after the last message.
            state.stop_token().cancel();
            run_on_stop(&on_stop);
            let reason = state
                .take_reason()
                .unwrap_or_else(|| messages::mcp::REMOTE_SESSION_ENDED.to_string());
            send_closed(&sink, &reason).await;
            on_end();
        }
    });
    if let Some(timeout) = spec.timeout {
        tokio::task::spawn_local(super::session::stop_when_idle(state.clone(), timeout));
    }
    RemoteSession { queue, state }
}

/// Send each server message from `out` to `sink`, in order, until the
/// session stops. Messages that wait in the queue at the stop still go to
/// the guest, if the guest takes them in a short time.
async fn write_sink(
    sink: &mcp_sink::Client,
    mut out: mpsc::Receiver<String>,
    state: &SessionState,
) {
    let stop = state.stop_token();
    let grace_over = sink_grace_over(stop.clone());
    tokio::pin!(grace_over);
    loop {
        let message = tokio::select! {
            biased;
            message = out.recv() => message,
            () = stop.cancelled() => None,
        };
        let Some(message) = message else {
            return;
        };
        state.touch();
        let mut request = sink.message_request();
        request.get().set_message(&message);
        // Wait for each call. This keeps the order, and a slow guest slows
        // the relay instead of filling host memory. After the stop, the
        // wait has a time limit, so that the session can end.
        let sent = tokio::select! {
            sent = request.send().promise => sent,
            () = &mut grace_over => {
                tracing::debug!("mcp sink: no answer after the stop");
                return;
            }
        };
        if let Err(e) = sent {
            // The guest cannot get messages any more. Thus the session has
            // no use.
            tracing::debug!("mcp sink: {e}");
            return;
        }
    }
}

impl mcp_session::Server for RemoteSession {
    async fn send(
        self: Rc<Self>,
        params: mcp_session::SendParams,
        _results: mcp_session::SendResults,
    ) -> Result<(), capnp::Error> {
        let message = params.get()?.get_message()?.to_str()?;
        // Same contract as a stdio session: one message for each line.
        if message.contains(['\n', '\r']) {
            return Err(capnp::Error::failed(
                messages::mcp::MESSAGE_HAS_NEWLINE.into(),
            ));
        }
        if self.state.is_stopped() {
            return Err(capnp::Error::failed(
                messages::mcp::REMOTE_SESSION_CLOSED.into(),
            ));
        }
        // Do not wait for room in the queue: a slow server must not block
        // the guest.
        self.queue.try_send(message.to_string()).map_err(|e| {
            capnp::Error::failed(match e {
                mpsc::error::TrySendError::Full(_) => messages::mcp::SEND_QUEUE_FULL.to_string(),
                mpsc::error::TrySendError::Closed(_) => {
                    messages::mcp::REMOTE_SESSION_CLOSED.to_string()
                }
            })
        })?;
        self.state.touch();
        Ok(())
    }
}

impl Drop for RemoteSession {
    fn drop(&mut self) {
        self.state.stop_token().cancel();
    }
}

/// The parts of a session that the transport tasks use to reach the guest.
#[derive(Clone)]
struct Relay {
    /// Name of the server, for the logs.
    name: String,
    /// Queue of the guest sink writer.
    out: mpsc::Sender<String>,
    /// Activity and stop signal of the session.
    state: Rc<SessionState>,
}

impl Relay {
    /// Send `message` to the guest, after the earlier messages.
    async fn deliver(&self, message: &Value) {
        // A full sink queue slows the server stream. After the stop, the
        // writer is gone and the message has no receiver.
        let _ = self.out.send(message.to_string()).await;
    }

    /// Give the guest an error response for each request in `ids`. Each
    /// item is the `id` of one request.
    async fn fail(&self, ids: impl IntoIterator<Item = Value>) {
        for id in ids {
            let error = serde_json::json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": {
                    "code": -32603,
                    "message": messages::mcp::REMOTE_REQUEST_FAILED,
                },
            });
            self.deliver(&error).await;
        }
    }

    /// Stop the session with `reason` for the guest.
    fn stop(&self, reason: String) {
        self.state.stop_with(reason);
    }

    /// Run `task` until it ends or the session stops.
    fn spawn(&self, task: impl Future<Output = ()> + 'static) {
        let stop = self.state.stop_token();
        tokio::task::spawn_local(async move {
            tokio::select! {
                () = task => {}
                () = stop.cancelled() => {}
            }
        });
    }
}

/// IDs of the requests in the guest message `message`: a request, or a
/// batch of messages. Notifications and responses have no reply, so they
/// give no ID.
fn request_ids(message: Option<&Value>) -> Vec<Value> {
    let request_id = |m: &Value| {
        m.get("method")?;
        m.get("id").filter(|id| !id.is_null()).cloned()
    };
    match message {
        Some(Value::Array(batch)) => batch.iter().filter_map(request_id).collect(),
        Some(m) => request_id(m).into_iter().collect(),
        None => Vec::new(),
    }
}

/// Remove from `pending` the IDs of the responses in the server message
/// `message`.
fn mark_answered(pending: &mut Vec<Value>, message: &Value) {
    let answered = |m: &Value| {
        m.get("method").is_none() && (m.get("result").is_some() || m.get("error").is_some())
    };
    let one = |pending: &mut Vec<Value>, m: &Value| {
        if answered(m)
            && let Some(id) = m.get("id")
        {
            pending.retain(|p| p != id);
        }
    };
    match message {
        Value::Array(batch) => batch.iter().for_each(|m| one(pending, m)),
        m => one(pending, m),
    }
}

/// Parse a server message. Only a JSON object or a batch array is valid.
fn parse_message(data: &[u8]) -> Option<Value> {
    serde_json::from_slice::<Value>(data)
        .ok()
        .filter(|v| v.is_object() || v.is_array())
}

/// Check if the media type of `response` is `expected`.
fn has_media_type(response: &reqwest::Response, expected: &str) -> bool {
    response
        .headers()
        .get(CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.split(';').next())
        .is_some_and(|v| v.trim().eq_ignore_ascii_case(expected))
}

/// Read the full body of `response`, at most [`MAX_MESSAGE`] bytes.
/// Returns:
///   The body, or the problem for the log.
async fn read_body(response: &mut reqwest::Response) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| e.without_url().to_string())?
    {
        if body.len() + chunk.len() > MAX_MESSAGE {
            return Err(format!("body longer than {MAX_MESSAGE} bytes"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// Problem of an event stream.
enum StreamError {
    /// A line or an event is too long. The session must stop.
    TooLong,
    /// The read failed. The text is for the log.
    Read(String),
}

/// Events of one response body.
struct EventStream {
    response: reqwest::Response,
    parser: sse::Parser,
    /// Set when the parser refused a part. The events before the problem
    /// still go to the caller first.
    too_long: bool,
}

impl EventStream {
    /// Read the events of `response`.
    fn new(response: reqwest::Response) -> Self {
        Self {
            response,
            parser: sse::Parser::new(MAX_MESSAGE),
            too_long: false,
        }
    }

    /// Read the next event.
    /// Returns:
    ///   The event, `None` at the end of the stream, or the problem.
    async fn next(&mut self) -> Result<Option<sse::Event>, StreamError> {
        loop {
            if let Some(event) = self.parser.next_event() {
                return Ok(Some(event));
            }
            if self.too_long {
                return Err(StreamError::TooLong);
            }
            let chunk = self
                .response
                .chunk()
                .await
                .map_err(|e| StreamError::Read(e.without_url().to_string()))?;
            let Some(chunk) = chunk else {
                return Ok(None);
            };
            self.too_long = self.parser.feed(&chunk).is_err();
        }
    }
}

/// Check if the event `kind` carries a JSON-RPC message. The default
/// event type is `message`.
fn is_message_event(kind: &str) -> bool {
    kind.is_empty() || kind == "message"
}
