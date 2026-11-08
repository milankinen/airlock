//! Message routing between the client and the sessions.
//!
//! Answers the client handshake itself. Starts one session handle for each
//! session request, and sends the later messages of a session to its
//! handle. The handle connects the session to its agent. Gives the agent
//! requests ids that are unique for the client, and gives each client
//! answer to its handle with the original id. A handle gets only the
//! messages of its own sessions. The end of a handle ends only that
//! handle. A session close stops the handle of the session, and a load of
//! an open session stops its old handle. The proxy runs until the client
//! closes or a host signal comes. Then it stops all handles and waits for
//! them.
//!
//! A session list with a session dir also starts a handle, as a session
//! request does. A request for a session that a list named and that has no
//! handle starts a handle in the session dir of the list.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use futures::StreamExt;
use futures::future::LocalBoxFuture;
use futures::stream::FuturesUnordered;
use serde_json::{Value, json};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::{Notify, mpsc};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use super::capabilities;
use super::rpc::{self, Kind, RequestId, SessionId, key, session_id, to_line};
use crate::messages;
use crate::runtime::SignalStream;

/// Settings of the router.
pub struct ProxyConfig {
    /// Agent name that the client shows.
    pub title: String,
    /// Version of airlock.
    pub version: String,
    /// Remove the client features that let the agent act on the host.
    pub restrict: bool,
    /// The agents bridge the editor MCP servers into the sandbox.
    pub proxy_mcp_servers: bool,
    /// Agent feature names that the handshake reply offers.
    pub agent_capabilities: Vec<String>,
    /// Source of the host signals, or `None` to not watch them. The proxy
    /// watches them from the first handle start. A signal stops all
    /// handles and the proxy. A second signal kills the agents.
    pub signals: Option<fn() -> anyhow::Result<SignalStream>>,
}

/// Identity of one session handle in the proxy.
pub type HandleId = u64;

/// Message from a session handle to the proxy.
pub enum HandleMessage {
    /// A message for the client.
    Client {
        /// The JSON-RPC message.
        msg: Value,
        /// The session that the message starts, if it is the OK response
        /// to the session request of the handle.
        claim: Option<Claim>,
    },
    /// The agent completed its handshake. A signal then lets the agent
    /// save the session.
    Ready,
    /// The handle ended. This is the last message of the handle.
    Ended,
}

/// Session that a handle starts with its session request.
pub struct Claim {
    /// The ACP session id.
    pub session: String,
    /// The client asked for this session id (load or resume). The handle
    /// then gets the session also if another handle has it.
    pub replace: bool,
}

/// One session, or one one-shot request, as the proxy sees it.
pub trait SessionHandle {
    /// Give the client message `msg` to the agent of the handle.
    /// Returns:
    ///   `false` if the handle ended and takes no more messages.
    fn send(&self, msg: Value) -> bool;

    /// Stop the handle and release its MCP servers. Takes the handle. A
    /// handle that ended already only gives its exit code.
    /// Returns:
    ///   A future of the exit code of the agent, 0 if it did not complete
    ///   its handshake.
    fn shutdown(self: Box<Self>) -> LocalBoxFuture<'static, i32>;
}

/// Fixed settings of all session handles of one proxy run.
pub struct SessionContext {
    /// Client `initialize` params after the filter.
    pub init_params: Value,
    /// `initialize` result that the proxy gave to the client.
    pub offered: Value,
    /// Destination of the handle messages.
    pub messages: mpsc::Sender<(HandleId, HandleMessage)>,
    /// Kills the agents at once in their stop.
    pub kill: CancellationToken,
}

/// Makes the session factory when the client handshake gives the init
/// params.
pub trait SessionFactoryInit {
    /// Make the factory.
    /// Args:
    ///  - `context`: Fixed settings of all handles
    fn init(self: Box<Self>, context: SessionContext) -> Rc<dyn SessionFactory>;
}

/// Starts the session handles.
pub trait SessionFactory {
    /// Start the handle of one session request. The factory gives the
    /// handle its identity. A failed start comes later as an error response
    /// and an end message of the handle.
    /// Args:
    ///  - `cwd`: Session dir of the session request on the host
    ///  - `first`: The session request. The agent gets it after its
    ///    handshake.
    ///
    /// Returns:
    ///   The identity and the new handle.
    fn create(&self, cwd: String, first: Value) -> (HandleId, Box<dyn SessionHandle>);

    /// Get the host session dir of a session that a list response of this
    /// factory named. A delete of the session removes it.
    /// Returns:
    ///   The session dir, or `None` if no recent list named the session.
    fn session_dir(&self, sid: &str) -> Option<String>;
}

/// Factory of a proxy run.
enum FactoryState {
    /// Waits for the client handshake. Has the sender for the handle
    /// messages.
    Pending(
        Box<dyn SessionFactoryInit>,
        mpsc::Sender<(HandleId, HandleMessage)>,
    ),
    /// Made at the client handshake.
    Ready(Rc<dyn SessionFactory>),
    /// The proxy stops. No new handle starts.
    Closed,
}

/// Maximum number of parts that wait for the client output. A full queue
/// stops the proxy until the client reads.
const WRITER_QUEUE: usize = 4;

/// Maximum number of client lines that wait for the proxy. A full queue
/// stops the read of the client input.
const READER_QUEUE: usize = 16;

/// Maximum number of handle messages that wait for the proxy.
const HANDLE_MESSAGES: usize = 16;

/// Run the proxy between a client and its session handles.
/// Args:
///  - `factory`: Makes the factory that starts a handle for each session
///    request, at the client handshake
///  - `config`: Proxy settings
///  - `input`: Client messages, one per line
///  - `output`: Destination of the messages to the client
///
/// Returns:
///   The first exit code that is not 0 of the handles, else 0. 128 + the
///   signal number if a signal came before a handle sent
///   [`HandleMessage::Ready`].
pub async fn run<R, W>(
    factory: Box<dyn SessionFactoryInit>,
    config: &ProxyConfig,
    input: R,
    output: W,
) -> i32
where
    R: AsyncBufRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
{
    let (out, out_task) = spawn_writer(output);
    let (messages, handle_messages) = mpsc::channel(HANDLE_MESSAGES);
    let proxy = Proxy {
        factory: RefCell::new(FactoryState::Pending(factory, messages)),
        config,
        out,
        kill: CancellationToken::new(),
        ended: CancellationToken::new(),
        stopping: RefCell::default(),
        stopped: Notify::new(),
        closing: Cell::new(false),
        handshake: RefCell::default(),
        started: Cell::new(false),
        handles: RefCell::default(),
        sessions: RefCell::default(),
        former: RefCell::default(),
        agent_requests: RefCell::default(),
        client_requests: RefCell::default(),
        ready: Cell::new(false),
        failed: Cell::new(None),
    };
    let (code, (), ()) = tokio::join!(
        proxy.to_handles(spawn_reader(input)),
        proxy.to_client(handle_messages),
        proxy.reap()
    );
    // Write all messages before the exit.
    drop(proxy);
    let _ = out_task.await;
    code
}

/// State of one proxy run. The client side and the handle side run at the
/// same time in one task. They never keep a borrow across an await.
struct Proxy<'a> {
    factory: RefCell<FactoryState>,
    config: &'a ProxyConfig,
    /// Lines to the client.
    out: mpsc::Sender<Vec<u8>>,
    /// Kills all agents in their stop.
    kill: CancellationToken,
    /// All handles ended. The handle side then handles the waiting messages
    /// and stops.
    ended: CancellationToken,
    /// Ends of the handles that stop. They are not in `handles`.
    stopping: RefCell<FuturesUnordered<LocalBoxFuture<'static, i32>>>,
    /// Wakes the reaper when a handle starts to stop.
    stopped: Notify,
    /// The proxy stops. The reaper ends when no handle is left.
    closing: Cell<bool>,
    /// `initialize` result of the first client handshake.
    handshake: RefCell<Option<Value>>,
    /// A handle started.
    started: Cell<bool>,
    handles: RefCell<HashMap<HandleId, Box<dyn SessionHandle>>>,
    /// Handle of each ACP session.
    sessions: RefCell<HashMap<SessionId, HandleId>>,
    /// ACP sessions of the handles that stop. Their last agent output in
    /// these sessions still goes to the client.
    former: RefCell<HashMap<HandleId, HashSet<SessionId>>>,
    /// Agent requests by their id for the client, with their handle and
    /// original id.
    agent_requests: RefCell<HashMap<RequestId, (HandleId, Value)>>,
    /// Handle of each open client request, by request id. The flag is set
    /// for a `session/close` request.
    client_requests: RefCell<HashMap<RequestId, (HandleId, bool)>>,
    /// A handle sent [`HandleMessage::Ready`].
    ready: Cell<bool>,
    /// First exit code that is not 0.
    failed: Cell<Option<i32>>,
}

impl Proxy<'_> {
    /// Handle the client lines until the client closes or a signal comes.
    /// Then stop all handles.
    /// Returns:
    ///   The exit code of the proxy.
    async fn to_handles(&self, mut client: mpsc::Receiver<Vec<u8>>) -> i32 {
        let mut signals = None;
        let mut signal_code = None;
        loop {
            tokio::select! {
                line = client.recv() => {
                    let Some(line) = line else {
                        tracing::info!("client closed");
                        break;
                    };
                    self.on_client_line(&line).await;
                }
                signum = next_signal(&mut signals) => {
                    tracing::info!(signum, "signal: stop the agents");
                    // Handles without a session have nothing to save.
                    if !self.ready.get() {
                        signal_code = Some(128 + signum);
                        self.kill.cancel();
                    }
                    break;
                }
            }
            // Before the first handle start, a signal stops the proxy as
            // usual.
            if signals.is_none() && self.started.get() {
                signals = Some(watch_signals(self.config.signals));
            }
        }
        self.shutdown(&mut signals).await;
        signal_code.or(self.failed.get()).unwrap_or(0)
    }

    /// Stop all handles and wait for them, also for the handles that stopped
    /// before. A signal kills them at once.
    async fn shutdown(&self, signals: &mut Option<SignalStream>) {
        *self.factory.borrow_mut() = FactoryState::Closed;
        let mut handles: Vec<HandleId> = self.handles.borrow().keys().copied().collect();
        handles.sort_unstable();
        for hid in handles {
            self.stop_handle(hid);
        }
        self.closing.set(true);
        self.stopped.notify_one();
        loop {
            tokio::select! {
                () = self.ended.cancelled() => return,
                signum = next_signal(signals) => {
                    tracing::info!(signum, "signal: kill the agents");
                    self.kill.cancel();
                }
            }
        }
    }

    /// Take the handle `hid` out of the routing and start its stop. Its
    /// open client requests stay until the handle ends. The agent can still
    /// answer them.
    fn stop_handle(&self, hid: HandleId) {
        let mut owned = HashSet::new();
        self.sessions.borrow_mut().retain(|sid, owner| {
            if *owner == hid {
                owned.insert(sid.clone());
            }
            *owner != hid
        });
        self.agent_requests
            .borrow_mut()
            .retain(|_, (owner, _)| *owner != hid);
        // The handle ends anyway. Its close response must not stop it again.
        for (owner, close) in self.client_requests.borrow_mut().values_mut() {
            if *owner == hid {
                *close = false;
            }
        }
        let Some(proxy) = self.handles.borrow_mut().remove(&hid) else {
            return;
        };
        tracing::info!(handle = hid, "stop the session handle");
        self.former.borrow_mut().insert(hid, owned);
        self.stopping.borrow_mut().push(proxy.shutdown());
        self.stopped.notify_one();
    }

    /// Wait for the end of each handle that stops, and keep its exit code.
    /// Ends when the proxy stops and no handle is left.
    async fn reap(&self) {
        loop {
            // A new handle in the set does not wake the set. The notify does.
            let next = std::future::poll_fn(|cx| self.stopping.borrow_mut().poll_next_unpin(cx));
            let next = tokio::select! {
                next = next => next,
                () = self.stopped.notified() => continue,
            };
            match next {
                Some(code) => {
                    tracing::info!(code, "session handle ended");
                    self.record(code);
                }
                None if self.closing.get() => break,
                None => self.stopped.notified().await,
            }
        }
        self.ended.cancel();
    }

    /// Handle one client line.
    async fn on_client_line(&self, line: &[u8]) {
        let Ok(msg) = serde_json::from_slice::<Value>(line) else {
            tracing::debug!("drop client line that is not JSON");
            return;
        };
        match rpc::kind(&msg) {
            Some(Kind::Request { id: req_id, method }) => {
                let (req_id, method) = (req_id.clone(), method.to_string());
                self.on_client_request(req_id, &method, msg).await;
            }
            Some(Kind::Response { id: req_id }) => {
                let Some((hid, original)) = self.agent_requests.borrow_mut().remove(&key(req_id))
                else {
                    tracing::debug!(id = %req_id, "drop client response without a request");
                    return;
                };
                let mut msg = msg;
                msg["id"] = original;
                self.deliver(hid, msg);
            }
            Some(Kind::CancelRequest { req_id }) => {
                // A cancel without a request id has no target.
                let hid = req_id.and_then(|req_id| {
                    self.client_requests.borrow().get(&key(req_id)).map(|r| r.0)
                });
                if let Some(hid) = hid {
                    self.deliver(hid, msg);
                } else {
                    tracing::debug!("drop client cancel without a target");
                }
            }
            Some(Kind::Notification { method }) => {
                let hid = session_id(&msg).and_then(|sid| self.sessions.borrow().get(sid).copied());
                if let Some(hid) = hid {
                    self.deliver(hid, msg);
                } else {
                    tracing::debug!(method, "drop client notification without a target");
                }
            }
            None => tracing::debug!("drop client message that is not JSON-RPC"),
        }
    }

    /// Handle one client request: answer it, start a handle for it or
    /// send it to the handle of its session.
    async fn on_client_request(&self, req_id: Value, method: &str, msg: Value) {
        match method {
            "initialize" => self.handle_initialize(&req_id, &msg).await,
            "session/new" | "session/fork" => self.handle_session_new(req_id, method, msg).await,
            "session/load" | "session/resume" => {
                self.handle_session_reopen(req_id, method, msg).await;
            }
            // The handle of the session stops when the agent answers.
            "session/close" => self.delegate_to_session(req_id, method, msg, true).await,
            "session/list" => self.handle_session_list(req_id, method, msg).await,
            "authenticate" | "logout" => self.reply_not_supported(&req_id, method).await,
            m if m.starts_with("providers/") => self.reply_not_supported(&req_id, method).await,
            _ => self.delegate_to_session(req_id, method, msg, false).await,
        }
    }

    /// Handle `initialize`: keep the first params for the agents and answer
    /// with the capabilities of the proxy.
    async fn handle_initialize(&self, req_id: &Value, msg: &Value) {
        let client = msg
            .pointer("/params/clientInfo/name")
            .and_then(Value::as_str);
        let caps = msg
            .pointer("/params/clientCapabilities")
            .cloned()
            .unwrap_or_default();
        tracing::info!(client, %caps, "initialize");
        // The agents do their handshake with the first params. A second
        // handshake must not change them.
        let known = self.handshake.borrow().clone();
        let result = known.unwrap_or_else(|| self.first_handshake(msg));
        self.reply(req_id, Ok(result)).await;
    }

    /// Filter the client params of the first `initialize` request `msg` for
    /// the agents, and make the factory with them.
    /// Returns:
    ///   The `initialize` result for the client.
    fn first_handshake(&self, msg: &Value) -> Value {
        let params = msg.get("params").cloned().unwrap_or(Value::Null);
        let params = capabilities::prepare_initialize_params(params, self.config.restrict);
        let caps = params
            .get("clientCapabilities")
            .cloned()
            .unwrap_or_default();
        tracing::info!(%caps, "client features for the agents");
        let result = self.initialize_result(&params);
        *self.handshake.borrow_mut() = Some(result.clone());
        self.init_factory(params, result.clone());
        result
    }

    /// Make the factory with the init `params` and the `offered` result,
    /// unless the proxy stops.
    fn init_factory(&self, params: Value, offered: Value) {
        let mut state = self.factory.borrow_mut();
        let FactoryState::Pending(init, messages) =
            std::mem::replace(&mut *state, FactoryState::Closed)
        else {
            return;
        };
        *state = FactoryState::Ready(init.init(SessionContext {
            init_params: params,
            offered,
            messages,
            kill: self.kill.clone(),
        }));
    }

    /// Handle `session/new` and `session/fork`: start a handle for the
    /// session, or refuse the request.
    async fn handle_session_new(&self, req_id: Value, method: &str, msg: Value) {
        let cwd = msg.pointer("/params/cwd").and_then(Value::as_str);
        let cwd = cwd.map(str::to_string);
        if let Err(error) = self.start_handle(&req_id, cwd, msg) {
            tracing::info!(method, "refuse session request: {}", error.1);
            self.reply(&req_id, Err(error)).await;
        }
    }

    /// Handle `session/load` and `session/resume`: stop the old handle of
    /// the session, then start a new handle for it.
    async fn handle_session_reopen(&self, req_id: Value, method: &str, msg: Value) {
        let old = session_id(&msg).and_then(|sid| self.sessions.borrow().get(sid).copied());
        if let Some(old) = old {
            self.stop_handle(old);
        }
        self.handle_session_new(req_id, method, msg).await;
    }

    /// Handle `session/list`: a handle in the sandbox of the session dir
    /// answers. Without a session dir, the client gets an empty list and no
    /// error. The proxy does not know the sandbox then, and an error makes
    /// the editor show a failure.
    async fn handle_session_list(&self, req_id: Value, method: &str, msg: Value) {
        if msg.pointer("/params/cwd").is_some_and(Value::is_string) {
            self.handle_session_new(req_id, method, msg).await;
        } else {
            self.reply(&req_id, Ok(json!({ "sessions": [] }))).await;
        }
    }

    /// Refuse the client request `method`. The login and model provider
    /// methods act on the whole agent, not on one session.
    async fn reply_not_supported(&self, req_id: &Value, method: &str) {
        tracing::debug!(
            method,
            "refuse client request that the proxy does not support"
        );
        let error = (
            rpc::METHOD_NOT_FOUND,
            messages::acp::unsupported_client_method(method),
        );
        self.reply(req_id, Err(error)).await;
    }

    /// Send the client request `msg` to the handle of its session. Refuse
    /// the request if it has no known session.
    /// Args:
    ///  - `closes`: The request closes the session
    async fn delegate_to_session(&self, req_id: Value, method: &str, msg: Value, closes: bool) {
        let Some(sid) = session_id(&msg) else {
            tracing::debug!(method, "refuse client request without a session");
            let error = (
                rpc::METHOD_NOT_FOUND,
                messages::acp::no_session_method(method),
            );
            self.reply(&req_id, Err(error)).await;
            return;
        };
        let hid = self.sessions.borrow().get(sid).copied();
        let Some(hid) = hid else {
            // A listed session can have no handle. A handle in the sandbox
            // of the list then answers.
            let dir = match &*self.factory.borrow() {
                FactoryState::Ready(factory) => factory.session_dir(sid),
                _ => None,
            };
            if let Some(cwd) = dir {
                if let Err(error) = self.start_handle(&req_id, Some(cwd), msg) {
                    tracing::info!(method, "refuse client request: {}", error.1);
                    self.reply(&req_id, Err(error)).await;
                }
                return;
            }
            let error = (rpc::INVALID_PARAMS, messages::acp::unknown_session(sid));
            self.reply(&req_id, Err(error)).await;
            return;
        };
        let entry = (hid, closes);
        self.client_requests
            .borrow_mut()
            .insert(key(&req_id), entry);
        if !self.deliver(hid, msg) {
            self.client_requests.borrow_mut().remove(&key(&req_id));
            let error = (
                rpc::INTERNAL_ERROR,
                messages::acp::SESSION_AGENT_EXITED.into(),
            );
            self.reply(&req_id, Err(error)).await;
        }
    }

    /// Start the handle of the client request `msg` with the id `req_id`.
    /// Args:
    ///  - `cwd`: Host session dir that selects the sandbox, or `None` if
    ///    the request has none
    ///
    /// Returns:
    ///   The error code and message if the request is not valid or the
    ///   proxy stops.
    fn start_handle(
        &self,
        req_id: &Value,
        cwd: Option<String>,
        msg: Value,
    ) -> Result<(), (i64, String)> {
        if matches!(*self.factory.borrow(), FactoryState::Pending(..)) {
            return Err((rpc::INVALID_REQUEST, messages::acp::NO_HANDSHAKE.into()));
        }
        let Some(cwd) = cwd else {
            return Err((rpc::INVALID_PARAMS, messages::acp::NO_SESSION_DIR.into()));
        };
        if !cwd.starts_with('/') {
            return Err((rpc::INVALID_PARAMS, messages::acp::cwd_not_absolute(&cwd)));
        }
        let FactoryState::Ready(factory) = &*self.factory.borrow() else {
            return Err((rpc::INTERNAL_ERROR, messages::acp::SHUTTING_DOWN.into()));
        };
        let factory = factory.clone();
        tracing::info!(cwd, "start a session handle");
        self.started.set(true);
        let (hid, proxy) = factory.create(cwd, msg);
        let entry = (hid, false);
        self.client_requests.borrow_mut().insert(key(req_id), entry);
        self.handles.borrow_mut().insert(hid, proxy);
        Ok(())
    }

    /// Give the client message `msg` to the handle `hid`.
    /// Returns:
    ///   `false` if the handle does not run.
    fn deliver(&self, hid: HandleId, msg: Value) -> bool {
        self.handles
            .borrow()
            .get(&hid)
            .is_some_and(|proxy| proxy.send(msg))
    }

    /// Handle the handle messages until the proxy stopped all handles.
    async fn to_client(&self, mut handle_messages: mpsc::Receiver<(HandleId, HandleMessage)>) {
        loop {
            tokio::select! {
                next = handle_messages.recv() => match next {
                    Some((hid, message)) => self.on_handle(hid, message).await,
                    None => return,
                },
                () = self.ended.cancelled() => break,
            }
        }
        // The last messages of the handles can still wait.
        handle_messages.close();
        while let Some((hid, message)) = handle_messages.recv().await {
            self.on_handle(hid, message).await;
        }
    }

    /// Handle one message of the handle `hid`.
    async fn on_handle(&self, hid: HandleId, message: HandleMessage) {
        match message {
            HandleMessage::Client { msg, claim } => self.on_handle_message(hid, msg, claim).await,
            HandleMessage::Ready => self.ready.set(true),
            HandleMessage::Ended => self.on_handle_end(hid),
        }
    }

    /// Send one agent message of the handle `hid` to the client. Rename the
    /// id of an agent request, and keep the handle to its own sessions.
    async fn on_handle_message(&self, hid: HandleId, mut msg: Value, claim: Option<Claim>) {
        let mut close = false;
        match rpc::kind(&msg) {
            Some(Kind::Response { id: req_id }) => {
                let req_id = req_id.clone();
                let mut requests = self.client_requests.borrow_mut();
                let Some(&(_, closes)) = requests.get(&key(&req_id)).filter(|r| r.0 == hid) else {
                    tracing::debug!(handle = hid, id = %req_id, "drop agent response without a request");
                    return;
                };
                requests.remove(&key(&req_id));
                drop(requests);
                close = closes;
                if let Some(claim) = claim
                    && !self.claim(hid, &claim)
                {
                    let message = messages::acp::session_id_conflict(&claim.session);
                    tracing::warn!(handle = hid, "refuse session: {message}");
                    msg = rpc::error_response(&req_id, rpc::INTERNAL_ERROR, &message);
                }
            }
            Some(Kind::Request { id: req_id, method }) => {
                if !self.owns(hid, &msg) {
                    tracing::debug!(
                        handle = hid,
                        method,
                        "drop agent request for another session"
                    );
                    return;
                }
                // Two handles can use the same id. The client must see
                // different ids.
                let proxy_req_id = Value::String(format!("{hid}-{}", key(req_id)));
                let entry = (hid, req_id.clone());
                self.agent_requests
                    .borrow_mut()
                    .insert(key(&proxy_req_id), entry);
                msg["id"] = proxy_req_id;
            }
            Some(Kind::CancelRequest { .. }) if !self.owns(hid, &msg) => {
                tracing::debug!(handle = hid, "drop agent cancel for another session");
                return;
            }
            Some(Kind::Notification { method }) if !self.owns(hid, &msg) => {
                tracing::debug!(
                    handle = hid,
                    method,
                    "drop agent message for another session"
                );
                return;
            }
            // Only the pass mode lets messages without a kind through, for
            // example a batch.
            Some(Kind::CancelRequest { .. } | Kind::Notification { .. }) | None => {}
        }
        let _ = self.out.send(to_line(&msg)).await;
        // The handle ran only for the closed session.
        if close {
            self.stop_handle(hid);
        }
    }

    /// Give the session of `claim` to `hid`. A handle that stops gets no
    /// session.
    /// Returns:
    ///   `false` if another handle has the session and the client did not
    ///   ask for it.
    fn claim(&self, hid: HandleId, claim: &Claim) -> bool {
        if !self.handles.borrow().contains_key(&hid) {
            return true;
        }
        let mut sessions = self.sessions.borrow_mut();
        if let Some(&owner) = sessions.get(&claim.session)
            && owner != hid
            && !claim.replace
        {
            return false;
        }
        sessions.insert(claim.session.clone(), hid);
        true
    }

    /// Check that the session of the agent message `msg` is a session of
    /// `hid`. A new session id becomes a session of `hid`, for example
    /// the session of a sub agent. A handle that stops keeps only the
    /// sessions that it had at its stop, and gets no new session.
    // A load of an open session stops the old handle and gives the session
    // to a new handle. The last output of the old handle must not take the
    // session back before the new handle claims it.
    fn owns(&self, hid: HandleId, msg: &Value) -> bool {
        let Some(sid) = session_id(msg) else {
            return true;
        };
        if !self.handles.borrow().contains_key(&hid) {
            let former = self.former.borrow();
            return former.get(&hid).is_some_and(|owned| owned.contains(sid));
        }
        *self
            .sessions
            .borrow_mut()
            .entry(sid.to_string())
            .or_insert(hid)
            == hid
    }

    /// Forget a handle that ended. The reaper gets its exit code.
    fn on_handle_end(&self, hid: HandleId) {
        self.client_requests
            .borrow_mut()
            .retain(|_, (owner, _)| *owner != hid);
        // A stopped handle is already out of the routing.
        self.stop_handle(hid);
        self.former.borrow_mut().remove(&hid);
    }

    /// Keep the agent exit code `code` if it is the first that is not 0.
    fn record(&self, code: i32) {
        if code != 0 && self.failed.get().is_none() {
            self.failed.set(Some(code));
        }
    }

    /// Send a response to the client.
    /// Args:
    ///  - `req_id`: Id of the client request
    ///  - `result`: Result, or the error code and message
    async fn reply(&self, req_id: &Value, result: Result<Value, (i64, String)>) {
        let msg = match result {
            Ok(result) => rpc::result_response(req_id, &result),
            Err((code, message)) => rpc::error_response(req_id, code, &message),
        };
        let _ = self.out.send(to_line(&msg)).await;
    }

    /// The `initialize` result that the proxy gives to the client.
    /// Args:
    ///  - `params`: Client `initialize` params after the filter
    fn initialize_result(&self, params: &Value) -> Value {
        capabilities::initialize_result(
            &self.config.title,
            &self.config.version,
            self.config.proxy_mcp_servers,
            &self.config.agent_capabilities,
            params,
        )
    }
}

/// Start the watch of the host signals.
/// Returns:
///   The signals, or a stream without signals if the watch fails or the
///   config has no source.
fn watch_signals(source: Option<fn() -> anyhow::Result<SignalStream>>) -> SignalStream {
    match source.map(|source| source()) {
        Some(Ok(signals)) => signals,
        Some(Err(e)) => {
            tracing::warn!("no signal watch: {e}");
            Box::pin(futures::stream::pending())
        }
        None => Box::pin(futures::stream::pending()),
    }
}

/// Wait for the next host signal. Waits forever if there are no signals.
async fn next_signal(signals: &mut Option<SignalStream>) -> i32 {
    match signals {
        Some(signals) => match signals.next().await {
            Some(signum) => signum,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Read the client lines in a task. When [`READER_QUEUE`] lines wait, the
/// read waits.
/// Returns:
///   Receiver of the lines without their newline. It closes at the end of
///   the input or at a read error.
fn spawn_reader<R: AsyncBufRead + Unpin + 'static>(mut input: R) -> mpsc::Receiver<Vec<u8>> {
    let (tx, rx) = mpsc::channel(READER_QUEUE);
    tokio::task::spawn_local(async move {
        loop {
            let mut line = Vec::new();
            match input.read_until(b'\n', &mut line).await {
                Ok(0) => return,
                Ok(_) => {
                    if line.last() == Some(&b'\n') {
                        line.pop();
                    }
                    if tx.send(line).await.is_err() {
                        return;
                    }
                }
                Err(e) => {
                    tracing::debug!("client read: {e}");
                    return;
                }
            }
        }
    });
    rx
}

/// Write the data from a channel to `output` in a task, in order.
/// Returns:
///   Sender of the data, and the task. The task ends when all senders drop
///   and the data is written.
// One task writes all data. Thus the messages do not mix. The queue has a
// small limit. A slow reader thus makes the writers wait, and the data does
// not collect in memory.
pub(super) fn spawn_writer<W: AsyncWrite + Unpin + 'static>(
    mut output: W,
) -> (mpsc::Sender<Vec<u8>>, JoinHandle<()>) {
    let (tx, mut rx) = mpsc::channel::<Vec<u8>>(WRITER_QUEUE);
    let task = tokio::task::spawn_local(async move {
        while let Some(data) = rx.recv().await {
            if output.write_all(&data).await.is_err() || output.flush().await.is_err() {
                // The reader is gone. Take the rest of the data and drop it.
                while rx.recv().await.is_some() {}
                return;
            }
        }
    });
    (tx, task)
}
