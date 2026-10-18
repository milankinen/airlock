//! Link between the HTTP handlers and the host MCP servers.
//!
//! One local task holds the host connection and all sessions. The HTTP
//! handlers run on other tasks and send commands to it. Each session sends
//! the messages of its server process to the open event streams of the
//! session.

use std::cell::RefCell;
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::rc::Rc;

use airlock_common::supervisor_capnp::{mcp_host, mcp_session, mcp_sink};
use rand::TryRng;
use rand::rngs::SysRng;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, info, warn};

/// Max number of server messages that wait for an event stream. When the
/// queue is full, the oldest message is dropped.
const QUEUE_LIMIT: usize = 256;

/// Buffer size of each event stream. It is larger than [`QUEUE_LIMIT`], so a
/// new stream can always take all queued messages.
const STREAM_BUFFER: usize = 2 * QUEUE_LIMIT;

/// Receiver of the server messages (JSON-RPC lines) of one event stream.
/// The stream ends when the receiver returns `None`.
pub type MessageStream = mpsc::Receiver<String>;

/// Why a bridge command failed.
#[derive(Debug)]
pub enum BridgeError {
    /// No session has this ID for this server name.
    UnknownSession,
    /// The session already has a standalone event stream.
    StreamOpen,
    /// A request with the same ID is still open in this session.
    DuplicateId,
    /// The host refused the call, or the host connection failed.
    Host(String),
}

/// Bridge commands from the HTTP handlers to the bridge task.
enum Command {
    Open {
        name: String,
        reply: oneshot::Sender<Result<String, BridgeError>>,
    },
    Request {
        session: String,
        name: String,
        id: String,
        message: String,
        reply: oneshot::Sender<Result<MessageStream, BridgeError>>,
    },
    Send {
        session: String,
        name: String,
        message: String,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    },
    Listen {
        session: String,
        name: String,
        reply: oneshot::Sender<Result<MessageStream, BridgeError>>,
    },
    Close {
        session: String,
        name: String,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    },
}

/// Handle to the bridge task. The handlers can share it between threads.
#[derive(Clone)]
pub struct Bridge {
    tx: mpsc::UnboundedSender<Command>,
}

impl Bridge {
    /// Start the bridge task. Call inside a `LocalSet`.
    /// Args:
    ///  - `host`: Host capability that starts the MCP server processes
    pub fn start(host: mcp_host::Client) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        tokio::task::spawn_local(run(rx, host));
        Self { tx }
    }

    /// Start a new process of server `name`.
    ///
    /// Returns:
    ///   ID of the new session.
    pub async fn open(&self, name: &str) -> Result<String, BridgeError> {
        let name = name.to_string();
        self.call(|reply| Command::Open { name, reply }).await
    }

    /// Send a JSON-RPC request to the server of a session.
    /// Args:
    ///  - `id`: JSON text of the request ID
    ///  - `message`: The request as one line of JSON
    ///
    /// Returns:
    ///   Event stream that ends after the response to the request.
    pub async fn request(
        &self,
        session: &str,
        name: &str,
        id: String,
        message: String,
    ) -> Result<MessageStream, BridgeError> {
        let (session, name) = (session.to_string(), name.to_string());
        self.call(|reply| Command::Request {
            session,
            name,
            id,
            message,
            reply,
        })
        .await
    }

    /// Send a JSON-RPC notification or response (one line of JSON) to the
    /// server of a session.
    pub async fn send(
        &self,
        session: &str,
        name: &str,
        message: String,
    ) -> Result<(), BridgeError> {
        let (session, name) = (session.to_string(), name.to_string());
        self.call(|reply| Command::Send {
            session,
            name,
            message,
            reply,
        })
        .await
    }

    /// Open the standalone event stream of a session.
    ///
    /// Returns:
    ///   Event stream that ends with the session.
    pub async fn listen(&self, session: &str, name: &str) -> Result<MessageStream, BridgeError> {
        let (session, name) = (session.to_string(), name.to_string());
        self.call(|reply| Command::Listen {
            session,
            name,
            reply,
        })
        .await
    }

    /// End a session. The host stops its server process.
    pub async fn close(&self, session: &str, name: &str) -> Result<(), BridgeError> {
        let (session, name) = (session.to_string(), name.to_string());
        self.call(|reply| Command::Close {
            session,
            name,
            reply,
        })
        .await
    }

    /// End a session and do not wait for the result. For use in `Drop`.
    pub fn close_later(&self, session: String, name: String) {
        let (reply, _) = oneshot::channel();
        let _ = self.tx.send(Command::Close {
            session,
            name,
            reply,
        });
    }

    /// Send one command to the bridge task and wait for its result.
    async fn call<T>(
        &self,
        command: impl FnOnce(oneshot::Sender<Result<T, BridgeError>>) -> Command,
    ) -> Result<T, BridgeError> {
        let (reply, result) = oneshot::channel();
        let stopped = || BridgeError::Host("the MCP bridge stopped".into());
        self.tx.send(command(reply)).map_err(|_| stopped())?;
        result.await.map_err(|_| stopped())?
    }
}

/// All sessions by session ID.
type Sessions = Rc<RefCell<HashMap<String, Session>>>;

/// One MCP session: one server process on the host.
struct Session {
    /// Server name from the URL path.
    name: String,
    /// Host-side process. `None` until the host has started it. The
    /// process stops when this client is released.
    process: Option<mcp_session::Client>,
    /// Event streams of the open requests, oldest first.
    requests: Vec<RequestStream>,
    /// Standalone event stream.
    listen: Option<mpsc::Sender<String>>,
    /// Server messages that wait for an event stream.
    queue: VecDeque<String>,
}

/// Event stream of one open request.
struct RequestStream {
    /// JSON text of the request ID.
    id: String,
    tx: mpsc::Sender<String>,
}

impl RequestStream {
    /// End the stream with a JSON-RPC error response, so the client does
    /// not wait for a response that does not come.
    fn fail(self, reason: &str) {
        let id: Value = serde_json::from_str(&self.id).unwrap_or(Value::Null);
        let error = json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": { "code": -32603, "message": reason },
        });
        let _ = self.tx.try_send(error.to_string());
    }
}

impl Session {
    /// Find the event stream for server message `message`. Put the message
    /// in the queue if no stream is open.
    /// Args:
    ///  - `response_id`: JSON text of the ID if the message is a response
    ///
    /// Returns:
    ///   Sender of the stream, or `None` if the message is queued or
    ///   dropped.
    fn route(&mut self, response_id: Option<&str>, message: &str) -> Option<mpsc::Sender<String>> {
        // A closed stream means that the HTTP client went away.
        self.requests.retain(|r| !r.tx.is_closed());
        if let Some(id) = response_id {
            let Some(pos) = self.requests.iter().position(|r| r.id == id) else {
                debug!("mcp: dropped a response with no open request");
                return None;
            };
            // The caller drops the last sender after the send, and that
            // ends the stream.
            return Some(self.requests.remove(pos).tx);
        }
        // The server gives no information about which request a message
        // belongs to. Thus the newest request stream is a best-effort
        // guess. For the same reason, queued messages go to the next stream
        // that opens, also if it is a request stream.
        if let Some(r) = self.requests.last() {
            return Some(r.tx.clone());
        }
        if let Some(tx) = self.listen.as_ref().filter(|tx| !tx.is_closed()) {
            return Some(tx.clone());
        }
        if self.queue.len() >= QUEUE_LIMIT {
            self.queue.pop_front();
            warn!("mcp: no event stream is open, dropped the oldest queued message");
        }
        self.queue.push_back(message.to_string());
        None
    }

    /// End the session: fail each open request with `reason`. Dropping the
    /// session ends all event streams and releases the server process.
    fn end(self, reason: &str) {
        for r in self.requests {
            r.fail(reason);
        }
    }

    /// Make a new event stream and move the queued messages into it.
    fn new_stream(&mut self) -> (mpsc::Sender<String>, MessageStream) {
        let (tx, rx) = mpsc::channel(STREAM_BUFFER);
        for message in self.queue.drain(..) {
            // The buffer is larger than the queue, so this cannot fail.
            let _ = tx.try_send(message);
        }
        (tx, rx)
    }
}

/// Get the session `id` of server `name` from `sessions`.
fn session<'a>(
    sessions: &'a mut HashMap<String, Session>,
    id: &str,
    name: &str,
) -> Result<&'a mut Session, BridgeError> {
    sessions
        .get_mut(id)
        .filter(|s| s.name == name && s.process.is_some())
        .ok_or(BridgeError::UnknownSession)
}

/// Run the bridge task until all handles are dropped. Each command runs in
/// its own local task, so a slow host call does not block other sessions.
async fn run(mut rx: mpsc::UnboundedReceiver<Command>, host: mcp_host::Client) {
    let sessions = Sessions::default();
    while let Some(command) = rx.recv().await {
        tokio::task::spawn_local(handle(command, sessions.clone(), host.clone()));
    }
}

/// Run one bridge command and send its result.
async fn handle(command: Command, sessions: Sessions, host: mcp_host::Client) {
    match command {
        Command::Open { name, reply } => {
            let result = open(&sessions, &host, name).await;
            // The HTTP client went away during the open. Nobody knows the
            // session ID, so end the session now.
            if let Err(Ok(sid)) = reply.send(result)
                && let Some(s) = sessions.borrow_mut().remove(&sid)
            {
                info!("mcp: {}: session dropped, the client went away", s.name);
            }
        }
        Command::Request {
            session: sid,
            name,
            id,
            message,
            reply,
        } => {
            let started = {
                let mut map = sessions.borrow_mut();
                session(&mut map, &sid, &name).and_then(|s| {
                    // A client can send the ID again after its stream broke.
                    s.requests.retain(|r| !r.tx.is_closed());
                    if s.requests.iter().any(|r| r.id == id) {
                        return Err(BridgeError::DuplicateId);
                    }
                    let (tx, rx) = s.new_stream();
                    s.requests.push(RequestStream { id: id.clone(), tx });
                    Ok((rx, s.process.clone()))
                })
            };
            let (rx, process) = match started {
                Ok(v) => v,
                Err(e) => {
                    let _ = reply.send(Err(e));
                    return;
                }
            };
            // Give the stream to the handler before the send. The server
            // can write messages before it reads the request. If nobody
            // reads them, the server can block.
            let _ = reply.send(Ok(rx));
            if let Err(e) = send(process, message).await {
                fail_request(&sessions, &sid, &id, &e);
            }
        }
        Command::Send {
            session: sid,
            name,
            message,
            reply,
        } => {
            let process =
                session(&mut sessions.borrow_mut(), &sid, &name).map(|s| s.process.clone());
            let result = match process {
                Ok(p) => send(p, message).await,
                Err(e) => Err(e),
            };
            let _ = reply.send(result);
        }
        Command::Listen {
            session: sid,
            name,
            reply,
        } => {
            let result = session(&mut sessions.borrow_mut(), &sid, &name).and_then(|s| {
                if s.listen.as_ref().is_some_and(|tx| !tx.is_closed()) {
                    return Err(BridgeError::StreamOpen);
                }
                let (tx, rx) = s.new_stream();
                s.listen = Some(tx);
                Ok(rx)
            });
            let _ = reply.send(result);
        }
        Command::Close {
            session: sid,
            name,
            reply,
        } => {
            let mut map = sessions.borrow_mut();
            let result = session(&mut map, &sid, &name).map(|_| ());
            if result.is_ok()
                && let Some(s) = map.remove(&sid)
            {
                // Dropping the session releases the process client and
                // ends all its event streams.
                s.end("the MCP session was deleted");
                info!("mcp: {name}: session closed by client");
            }
            let _ = reply.send(result);
        }
    }
}

/// Start a server process on the host and add its session.
///
/// Returns:
///   ID of the new session.
async fn open(
    sessions: &Sessions,
    host: &mcp_host::Client,
    name: String,
) -> Result<String, BridgeError> {
    let sid = new_session_id()?;
    // Add the session before the call. The server can write messages before
    // the call returns, and they must go to the queue of the session.
    sessions.borrow_mut().insert(
        sid.clone(),
        Session {
            name: name.clone(),
            process: None,
            requests: Vec::new(),
            listen: None,
            queue: VecDeque::new(),
        },
    );
    let mut req = host.open_request();
    req.get().set_name(&name);
    req.get().set_sink(capnp_rpc::new_client(Sink {
        sessions: sessions.clone(),
        sid: sid.clone(),
    }));
    let process = async { req.send().promise.await?.get()?.get_session() }.await;
    let mut map = sessions.borrow_mut();
    match (process, map.get_mut(&sid)) {
        (Ok(process), Some(s)) => {
            s.process = Some(process);
            info!("mcp: {name}: session opened");
            Ok(sid)
        }
        // The process stopped before the call returned.
        (Ok(_), None) => Err(BridgeError::Host(format!("MCP server {name} stopped"))),
        (Err(e), _) => {
            map.remove(&sid);
            warn!("mcp: {name}: host refused to start the server: {e}");
            Err(BridgeError::Host(e.to_string()))
        }
    }
}

/// Write one message to the stdin of a server process.
async fn send(process: Option<mcp_session::Client>, message: String) -> Result<(), BridgeError> {
    let process = process.ok_or(BridgeError::UnknownSession)?;
    let mut req = process.send_request();
    req.get().set_message(&message);
    req.send()
        .promise
        .await
        .map(|_| ())
        .map_err(|e| BridgeError::Host(e.to_string()))
}

/// End the event stream of request `id` with a JSON-RPC error, because the
/// request did not get to the server.
fn fail_request(sessions: &Sessions, sid: &str, id: &str, err: &BridgeError) {
    let mut map = sessions.borrow_mut();
    let Some(s) = map.get_mut(sid) else { return };
    let Some(pos) = s.requests.iter().position(|r| r.id == id) else {
        return;
    };
    let stream = s.requests.remove(pos);
    warn!("mcp: {}: send to server failed: {err:?}", s.name);
    stream.fail("the MCP server did not get the request");
}

/// Make a new session ID: 128 random bits from the OS, in hex.
fn new_session_id() -> Result<String, BridgeError> {
    let mut bytes = [0u8; 16];
    SysRng
        .try_fill_bytes(&mut bytes)
        .map_err(|e| BridgeError::Host(format!("no random session ID: {e}")))?;
    Ok(bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    }))
}

/// Get the JSON text of the ID if `message` is a JSON-RPC response.
fn response_id(message: &str) -> Option<String> {
    let Ok(Value::Object(obj)) = serde_json::from_str::<Value>(message) else {
        return None;
    };
    if obj.contains_key("method") || !(obj.contains_key("result") || obj.contains_key("error")) {
        return None;
    }
    obj.get("id").map(Value::to_string)
}

/// Receiver of the messages of one server process.
struct Sink {
    sessions: Sessions,
    sid: String,
}

impl mcp_sink::Server for Sink {
    async fn message(
        self: Rc<Self>,
        params: mcp_sink::MessageParams,
        _results: mcp_sink::MessageResults,
    ) -> Result<(), capnp::Error> {
        let mut message = params.get()?.get_message()?.to_str()?.to_string();
        let response_id = response_id(&message);
        loop {
            let tx = {
                let mut map = self.sessions.borrow_mut();
                let Some(s) = map.get_mut(&self.sid) else {
                    return Ok(());
                };
                s.route(response_id.as_deref(), &message)
            };
            let Some(tx) = tx else { return Ok(()) };
            // The wait for buffer space slows down the host relay when an
            // HTTP client reads slowly (backpressure).
            match tx.send(message).await {
                Ok(()) => return Ok(()),
                // The HTTP client of the request went away.
                Err(_) if response_id.is_some() => return Ok(()),
                // The stream closed during the wait. Find a new one.
                Err(mpsc::error::SendError(m)) => message = m,
            }
        }
    }

    async fn closed(
        self: Rc<Self>,
        params: mcp_sink::ClosedParams,
        _results: mcp_sink::ClosedResults,
    ) -> Result<(), capnp::Error> {
        let reason = params.get()?.get_reason()?.to_str()?.to_string();
        // Dropping the session ends all its event streams. Later requests
        // with its ID get HTTP 404, so the client starts a new session.
        let session = self.sessions.borrow_mut().remove(&self.sid);
        if let Some(s) = session {
            info!("mcp: {}: server stopped: {reason}", s.name);
            s.end("the MCP server stopped");
        }
        Ok(())
    }
}
