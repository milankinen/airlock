//! Link between the HTTP handlers and the host MCP servers.
//!
//! One local task holds the routes, their host connections and all
//! sessions. The HTTP handlers run on other tasks and send commands to it.
//! The supervisor adds and removes routes through a second, local handle.
//! Each session sends the messages of its server process to the open event
//! streams of the session.
//!
//! The routes from the boot config stay for the life of the VM. The host
//! can add routes at run time and remove them again. The removal of a route
//! ends all its sessions.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fmt::{self, Write as _};
use std::rc::Rc;

use airlock_common::supervisor_capnp::{mcp_host, mcp_session, mcp_sink};
use rand::TryRng;
use rand::rngs::SysRng;
use serde_json::{Value, json};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Max number of server messages that wait for an event stream. When the
/// queue is full, the oldest message is dropped.
const QUEUE_LIMIT: usize = 256;

/// Buffer size of each event stream. It is larger than [`QUEUE_LIMIT`], so a
/// new stream can always take all queued messages.
const STREAM_BUFFER: usize = 2 * QUEUE_LIMIT;

/// Max number of routes that the host can add at run time.
const RUNTIME_LIMIT: usize = 256;

/// Max length of each part of a run-time route.
const PART_LIMIT: usize = 64;

/// Receiver of the server messages (JSON-RPC lines) of one event stream.
/// The stream ends when the receiver returns `None`.
pub type MessageStream = mpsc::Receiver<String>;

/// URL path of one MCP server.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Route {
    /// Host session part of a run-time route. `None` for a boot config
    /// route.
    pub session: Option<String>,
    /// Server name.
    pub name: String,
}

impl fmt::Display for Route {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.session {
            Some(session) => write!(f, "{session}/{}", self.name),
            None => f.write_str(&self.name),
        }
    }
}

/// Why a bridge command failed.
#[derive(Debug)]
pub enum BridgeError {
    /// No server has this route.
    UnknownRoute,
    /// No session has this ID for this route.
    UnknownSession,
    /// The session already has a standalone event stream.
    StreamOpen,
    /// A request with the same ID is still open in this session.
    DuplicateId,
    /// The host refused the call, or the host connection failed.
    Host(String),
}

/// Why the bridge refused a run-time route.
#[derive(Debug)]
pub enum AddError {
    /// The session or the name is empty, too long or has a bad character.
    BadPart,
    /// The route exists already.
    Duplicate,
    /// The route limit is reached.
    Full,
    /// The bridge task stopped.
    Stopped,
}

impl fmt::Display for AddError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::BadPart => "session and name must have 1-64 characters of [A-Za-z0-9_-]",
            Self::Duplicate => "the MCP server exists already",
            Self::Full => "too many MCP servers",
            Self::Stopped => "the MCP bridge stopped",
        })
    }
}

/// Bridge commands from the HTTP handlers to the bridge task.
enum Command {
    Exists {
        route: Route,
        reply: oneshot::Sender<bool>,
    },
    List {
        runtime: bool,
        reply: oneshot::Sender<Vec<String>>,
    },
    Open {
        route: Route,
        reply: oneshot::Sender<Result<String, BridgeError>>,
    },
    Request {
        session: String,
        route: Route,
        id: String,
        message: String,
        reply: oneshot::Sender<Result<MessageStream, BridgeError>>,
    },
    Send {
        session: String,
        route: Route,
        message: String,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    },
    Listen {
        session: String,
        route: Route,
        reply: oneshot::Sender<Result<MessageStream, BridgeError>>,
    },
    Close {
        session: String,
        route: Route,
        reply: oneshot::Sender<Result<(), BridgeError>>,
    },
}

/// Route commands from the supervisor to the bridge task. They hold host
/// capabilities, so they stay on the local thread.
enum RouteCommand {
    Add {
        route: Route,
        host: mcp_host::Client,
        /// `None` for a boot config route, which nobody removes.
        reply: Option<oneshot::Sender<Result<u64, AddError>>>,
    },
    Remove {
        route: Route,
        id: u64,
        /// Gets a value after the sessions of the route ended.
        reply: oneshot::Sender<()>,
    },
}

/// Handle to the bridge task for the HTTP handlers. The handlers can share
/// it between threads.
#[derive(Clone)]
pub struct Bridge {
    tx: mpsc::UnboundedSender<Command>,
}

/// Handle to the bridge task that adds and removes routes. Use it only on
/// the thread of the bridge task.
#[derive(Clone)]
pub struct Routes {
    tx: mpsc::UnboundedSender<RouteCommand>,
}

/// Start the bridge task with no routes. Call inside a `LocalSet`.
///
/// Returns:
///   The handle for the HTTP handlers and the handle for route changes.
pub fn start() -> (Bridge, Routes) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (route_tx, route_rx) = mpsc::unbounded_channel();
    tokio::task::spawn_local(run(rx, route_rx));
    (Bridge { tx }, Routes { tx: route_tx })
}

impl Bridge {
    /// Check if a server has `route`.
    pub async fn exists(&self, route: &Route) -> bool {
        let route = route.clone();
        let (reply, result) = oneshot::channel();
        let _ = self.tx.send(Command::Exists { route, reply });
        result.await.unwrap_or(false)
    }

    /// List the routes of the boot config (`runtime` false) or of the
    /// run-time servers (`runtime` true), oldest first.
    pub async fn list(&self, runtime: bool) -> Vec<String> {
        let (reply, result) = oneshot::channel();
        let _ = self.tx.send(Command::List { runtime, reply });
        result.await.unwrap_or_default()
    }

    /// Start a new process of the server at `route`.
    ///
    /// Returns:
    ///   ID of the new session.
    pub async fn open(&self, route: &Route) -> Result<String, BridgeError> {
        let route = route.clone();
        self.call(|reply| Command::Open { route, reply }).await
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
        route: &Route,
        id: String,
        message: String,
    ) -> Result<MessageStream, BridgeError> {
        let (session, route) = (session.to_string(), route.clone());
        self.call(|reply| Command::Request {
            session,
            route,
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
        route: &Route,
        message: String,
    ) -> Result<(), BridgeError> {
        let (session, route) = (session.to_string(), route.clone());
        self.call(|reply| Command::Send {
            session,
            route,
            message,
            reply,
        })
        .await
    }

    /// Open the standalone event stream of a session.
    ///
    /// Returns:
    ///   Event stream that ends with the session.
    pub async fn listen(&self, session: &str, route: &Route) -> Result<MessageStream, BridgeError> {
        let (session, route) = (session.to_string(), route.clone());
        self.call(|reply| Command::Listen {
            session,
            route,
            reply,
        })
        .await
    }

    /// End a session. The host stops its server process.
    pub async fn close(&self, session: &str, route: &Route) -> Result<(), BridgeError> {
        let (session, route) = (session.to_string(), route.clone());
        self.call(|reply| Command::Close {
            session,
            route,
            reply,
        })
        .await
    }

    /// End a session and do not wait for the result. For use in `Drop`.
    pub fn close_later(&self, session: String, route: Route) {
        let (reply, _) = oneshot::channel();
        let _ = self.tx.send(Command::Close {
            session,
            route,
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

impl Routes {
    /// Add the servers of the boot config at `http://mcp.airlock/<name>`.
    /// They stay for the life of the VM.
    /// Args:
    ///  - `host`: Host capability that starts the server processes
    ///  - `names`: Names of the servers that the host grants
    pub fn add_boot(&self, host: &mcp_host::Client, names: Vec<String>) {
        for name in names {
            let route = Route {
                session: None,
                name,
            };
            let _ = self.tx.send(RouteCommand::Add {
                route,
                host: host.clone(),
                reply: None,
            });
        }
    }

    /// Add a run-time server at `http://acp.mcp.airlock/<session>/<name>`.
    /// Args:
    ///  - `host`: Host capability that starts the server processes
    ///
    /// Returns:
    ///   The route and the ID of this registration. Give both to
    ///   [`Routes::remove`].
    pub async fn add(
        &self,
        session: &str,
        name: &str,
        host: mcp_host::Client,
    ) -> Result<(Route, u64), AddError> {
        if !valid_part(session) || !valid_part(name) {
            return Err(AddError::BadPart);
        }
        let route = Route {
            session: Some(session.to_string()),
            name: name.to_string(),
        };
        let (reply, result) = oneshot::channel();
        self.tx
            .send(RouteCommand::Add {
                route: route.clone(),
                host,
                reply: Some(reply),
            })
            .map_err(|_| AddError::Stopped)?;
        let id = result.await.map_err(|_| AddError::Stopped)??;
        Ok((route, id))
    }

    /// Remove registration `id` of `route` and end all its sessions. Does
    /// nothing if a newer registration has the route now.
    ///
    /// Returns:
    ///   A receiver that gets a value after the sessions ended. Drop it to
    ///   not wait.
    pub fn remove(&self, route: Route, id: u64) -> oneshot::Receiver<()> {
        let (reply, done) = oneshot::channel();
        let _ = self.tx.send(RouteCommand::Remove { route, id, reply });
        done
    }
}

/// Check that a part of a run-time route has 1-64 characters of
/// `[A-Za-z0-9_-]`.
fn valid_part(part: &str) -> bool {
    (1..=PART_LIMIT).contains(&part.len())
        && part
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// One registered server.
struct Server {
    /// Registration ID. A removal must give the same ID, so a late removal
    /// cannot remove a newer server of the same route.
    id: u64,
    /// Host capability that starts the server processes.
    host: mcp_host::Client,
    /// Each open in flight holds a clone. The receiver sees the end of the
    /// channel when the server is removed and all opens completed.
    opens: mpsc::Sender<()>,
    opens_done: mpsc::Receiver<()>,
}

/// State of the bridge task.
#[derive(Default)]
struct State {
    /// All servers by route.
    servers: RefCell<HashMap<Route, Server>>,
    /// All sessions by session ID.
    sessions: RefCell<HashMap<String, Session>>,
    /// Last registration ID.
    last_id: Cell<u64>,
}

/// One MCP session: one server process on the host.
struct Session {
    /// Route of the server.
    route: Route,
    /// Registration ID of the server.
    server: u64,
    /// Host-side process. `None` until the host has started it. The
    /// process stops when this client is released.
    process: Option<mcp_session::Client>,
    /// Event streams of the open requests, oldest first.
    requests: Vec<RequestStream>,
    /// Standalone event stream.
    listen: Option<mpsc::Sender<String>>,
    /// Server messages that wait for an event stream.
    queue: VecDeque<String>,
    /// Fires when the session is dropped. A host message that waits for
    /// room in an event stream then stops the wait.
    ended: CancellationToken,
}

impl Drop for Session {
    fn drop(&mut self) {
        // A guest client can keep a stream open and not read it. The host
        // call for the next message must not wait for it forever.
        self.ended.cancel();
    }
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
    fn end(mut self, reason: &str) {
        for r in std::mem::take(&mut self.requests) {
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

/// Get the session `id` of `route` from `sessions`.
fn session<'a>(
    sessions: &'a mut HashMap<String, Session>,
    id: &str,
    route: &Route,
) -> Result<&'a mut Session, BridgeError> {
    sessions
        .get_mut(id)
        .filter(|s| s.route == *route && s.process.is_some())
        .ok_or(BridgeError::UnknownSession)
}

/// Run the bridge task until all handles are dropped. Each HTTP command
/// runs in its own local task, so a slow host call does not block other
/// sessions. Route changes run in order of arrival.
async fn run(
    mut rx: mpsc::UnboundedReceiver<Command>,
    mut route_rx: mpsc::UnboundedReceiver<RouteCommand>,
) {
    let state = Rc::new(State::default());
    loop {
        tokio::select! {
            Some(command) = rx.recv() => {
                tokio::task::spawn_local(handle(command, state.clone()));
            }
            Some(command) = route_rx.recv() => change_route(command, &state),
            else => break,
        }
    }
}

/// Run one route command. A removal ends the sessions of the route at once
/// and sends its reply when the opens in flight completed. Thus a host
/// that does not complete an open also blocks the removal reply.
fn change_route(command: RouteCommand, state: &State) {
    match command {
        RouteCommand::Add { route, host, reply } => {
            let mut servers = state.servers.borrow_mut();
            let result = if servers.contains_key(&route) {
                Err(AddError::Duplicate)
            } else if route.session.is_some()
                && servers.keys().filter(|r| r.session.is_some()).count() >= RUNTIME_LIMIT
            {
                Err(AddError::Full)
            } else {
                let id = state.last_id.get() + 1;
                state.last_id.set(id);
                let (opens, opens_done) = mpsc::channel(1);
                info!("mcp: {route}: server added");
                servers.insert(
                    route.clone(),
                    Server {
                        id,
                        host,
                        opens,
                        opens_done,
                    },
                );
                Ok(id)
            };
            if let Err(e) = &result {
                warn!("mcp: {route}: server refused: {e}");
            }
            // The caller went away before the reply. Nobody holds a grant,
            // so remove the server again. It has no sessions yet.
            if let Some(reply) = reply
                && let Err(Ok(_)) = reply.send(result)
            {
                servers.remove(&route);
                info!("mcp: {route}: server removed, the caller went away");
            }
        }
        RouteCommand::Remove { route, id, reply } => {
            let removed = {
                let mut servers = state.servers.borrow_mut();
                match servers.get(&route) {
                    Some(s) if s.id == id => servers.remove(&route),
                    _ => None,
                }
            };
            let Some(server) = removed else {
                let _ = reply.send(());
                return;
            };
            // Opens in flight see that their session is gone when the host
            // returns, and release the new process.
            let ended: Vec<Session> = {
                let mut sessions = state.sessions.borrow_mut();
                let ids: Vec<String> = sessions
                    .iter()
                    .filter(|(_, s)| s.route == route && s.server == id)
                    .map(|(sid, _)| sid.clone())
                    .collect();
                ids.iter().filter_map(|sid| sessions.remove(sid)).collect()
            };
            info!(
                "mcp: {route}: server removed, {} session(s) ended",
                ended.len()
            );
            for s in ended {
                s.end("the MCP server was removed");
            }
            let Server {
                opens,
                mut opens_done,
                ..
            } = server;
            drop(opens);
            tokio::task::spawn_local(async move {
                // `None` comes when the last open in flight completed.
                let _ = opens_done.recv().await;
                let _ = reply.send(());
            });
        }
    }
}

/// Run one HTTP command and send its result.
async fn handle(command: Command, state: Rc<State>) {
    match command {
        Command::Exists { route, reply } => {
            let _ = reply.send(state.servers.borrow().contains_key(&route));
        }
        Command::List { runtime, reply } => {
            let servers = state.servers.borrow();
            let mut routes: Vec<(u64, String)> = servers
                .iter()
                .filter(|(r, _)| r.session.is_some() == runtime)
                .map(|(r, s)| (s.id, r.to_string()))
                .collect();
            routes.sort_unstable();
            let _ = reply.send(routes.into_iter().map(|(_, r)| r).collect());
        }
        Command::Open { route, reply } => {
            let result = open(&state, route).await;
            // The HTTP client went away during the open. Nobody knows the
            // session ID, so end the session now.
            if let Err(Ok(sid)) = reply.send(result)
                && let Some(s) = state.sessions.borrow_mut().remove(&sid)
            {
                info!("mcp: {}: session dropped, the client went away", s.route);
            }
        }
        Command::Request {
            session: sid,
            route,
            id,
            message,
            reply,
        } => {
            let started = {
                let mut map = state.sessions.borrow_mut();
                session(&mut map, &sid, &route).and_then(|s| {
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
                fail_request(&state, &sid, &id, &e);
            }
        }
        Command::Send {
            session: sid,
            route,
            message,
            reply,
        } => {
            let process =
                session(&mut state.sessions.borrow_mut(), &sid, &route).map(|s| s.process.clone());
            let result = match process {
                Ok(p) => send(p, message).await,
                Err(e) => Err(e),
            };
            let _ = reply.send(result);
        }
        Command::Listen {
            session: sid,
            route,
            reply,
        } => {
            let result = session(&mut state.sessions.borrow_mut(), &sid, &route).and_then(|s| {
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
            route,
            reply,
        } => {
            let mut map = state.sessions.borrow_mut();
            let result = session(&mut map, &sid, &route).map(|_| ());
            if result.is_ok()
                && let Some(s) = map.remove(&sid)
            {
                // Dropping the session releases the process client and
                // ends all its event streams.
                s.end("the MCP session was deleted");
                info!("mcp: {route}: session closed by client");
            }
            let _ = reply.send(result);
        }
    }
}

/// Start a server process on the host and add its session.
///
/// Returns:
///   ID of the new session.
async fn open(state: &Rc<State>, route: Route) -> Result<String, BridgeError> {
    let (host, server, opens) = {
        let servers = state.servers.borrow();
        let s = servers.get(&route).ok_or(BridgeError::UnknownRoute)?;
        (s.host.clone(), s.id, s.opens.clone())
    };
    let sid = new_session_id()?;
    // Add the session before the call. The server can write messages before
    // the call returns, and they must go to the queue of the session.
    state.sessions.borrow_mut().insert(
        sid.clone(),
        Session {
            route: route.clone(),
            server,
            process: None,
            requests: Vec::new(),
            listen: None,
            queue: VecDeque::new(),
            ended: CancellationToken::new(),
        },
    );
    let mut req = host.open_request();
    req.get().set_name(&route.name);
    req.get().set_sink(capnp_rpc::new_client(Sink {
        state: state.clone(),
        sid: sid.clone(),
    }));
    let process = async { req.send().promise.await?.get()?.get_session() }.await;
    let mut map = state.sessions.borrow_mut();
    let result = match (process, map.get_mut(&sid)) {
        (Ok(process), Some(s)) => {
            s.process = Some(process);
            info!("mcp: {route}: session opened");
            Ok(sid)
        }
        // The host removed the server during the open. Dropping the
        // process client stops the process.
        (_, None) if !registered(state, &route, server) => Err(BridgeError::UnknownRoute),
        // The process stopped before the call returned.
        (Ok(_), None) => Err(BridgeError::Host(format!("MCP server {route} stopped"))),
        (Err(e), _) => {
            map.remove(&sid);
            warn!("mcp: {route}: host refused to start the server: {e}");
            Err(BridgeError::Host(e.to_string()))
        }
    };
    // Tell a waiting removal that this open completed. The process client
    // of a removed server is dropped at this point.
    drop(opens);
    result
}

/// Check if registration `id` still has `route`.
fn registered(state: &State, route: &Route, id: u64) -> bool {
    state
        .servers
        .borrow()
        .get(route)
        .is_some_and(|s| s.id == id)
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
fn fail_request(state: &State, sid: &str, id: &str, err: &BridgeError) {
    let mut map = state.sessions.borrow_mut();
    let Some(s) = map.get_mut(sid) else { return };
    let Some(pos) = s.requests.iter().position(|r| r.id == id) else {
        return;
    };
    let stream = s.requests.remove(pos);
    warn!("mcp: {}: send to server failed: {err:?}", s.route);
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
    state: Rc<State>,
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
            let (tx, ended) = {
                let mut map = self.state.sessions.borrow_mut();
                let Some(s) = map.get_mut(&self.sid) else {
                    return Ok(());
                };
                (s.route(response_id.as_deref(), &message), s.ended.clone())
            };
            let Some(tx) = tx else { return Ok(()) };
            // The wait for buffer space slows down the host relay when an
            // HTTP client reads slowly (backpressure). The end of the
            // session stops the wait, and the message has no receiver.
            let sent = tokio::select! {
                sent = tx.send(message) => sent,
                () = ended.cancelled() => return Ok(()),
            };
            match sent {
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
        let session = self.state.sessions.borrow_mut().remove(&self.sid);
        if let Some(s) = session {
            info!("mcp: {}: server stopped: {reason}", s.route);
            s.end("the MCP server stopped");
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use crate::test_cfg::block_on_local;

    /// Test that the end of a session stops a host message that waits for
    /// room in an event stream. A guest client that keeps a stream open and
    /// does not read it must not block the host relay forever.
    ///   1. Add a session with a full standalone stream that nobody reads
    ///   2. Send a host message to the sink and check that it waits
    ///   3. Remove the session and check that the sink call returns
    #[test]
    fn removed_session_with_full_stream_ends_waiting_message() {
        block_on_local(async {
            let state = Rc::new(State::default());
            let (listen, _unread) = mpsc::channel(1);
            listen.try_send("full".to_string()).unwrap();
            state.sessions.borrow_mut().insert(
                "s1".into(),
                Session {
                    route: Route {
                        session: None,
                        name: "echo".into(),
                    },
                    server: 1,
                    process: None,
                    requests: Vec::new(),
                    listen: Some(listen),
                    queue: VecDeque::new(),
                    ended: CancellationToken::new(),
                },
            );
            let sink: mcp_sink::Client = capnp_rpc::new_client(Sink {
                state: state.clone(),
                sid: "s1".into(),
            });
            let mut request = sink.message_request();
            request
                .get()
                .set_message(r#"{"jsonrpc":"2.0","method":"note"}"#);
            let mut call = std::pin::pin!(request.send().promise);
            let waited = tokio::time::timeout(Duration::from_millis(100), &mut call).await;
            assert!(waited.is_err(), "the call must wait for room");

            drop(state.sessions.borrow_mut().remove("s1"));
            tokio::time::timeout(Duration::from_secs(5), call)
                .await
                .expect("the call returns after the session end")
                .unwrap();
        });
    }
}
