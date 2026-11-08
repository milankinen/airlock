//! Message routing between the client and the sessions.
//!
//! Answers the client handshake itself. Starts one session handle for each
//! session request, and sends the later messages of a session to its
//! handle. The handle connects the session to its agent. The ids of the
//! agent requests name their handle. A client answer goes to that handle.
//! A handle gets only the messages of its own sessions. An agent request
//! in another session gets an error. The number of new sessions of a
//! handle has a limit. The end of a handle ends only that handle. A load of
//! an open session gives the session to a new handle when the new handle
//! accepts the load, and then stops the old handle. The proxy runs until
//! the client closes or a host signal comes. Then it stops all handles and
//! waits for them.
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

use super::rpc::{self, Kind, RequestId, SessionId, key, session_id, to_line};
use super::{Config, capabilities};
use crate::messages;
use crate::runtime::SignalStream;

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
    /// The session request of the handle, a load or a resume of `session`,
    /// passed the checks of the handle and goes to the agent. The handle
    /// gets the session now, and the old handle of the session stops.
    Opening {
        /// The ACP session id.
        session: SessionId,
    },
    /// The agent of the handle ended, and the handle waits without an
    /// agent for its next request. A signal has no agent to let save.
    Idle,
    /// The handle ended. This is the last message of the handle.
    Ended,
}

/// Session that a handle starts with its session request.
pub struct Claim {
    /// The ACP session id.
    pub session: String,
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

/// Fixed settings of all session handles of one proxy run. The proxy makes
/// them at the client handshake.
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

/// Starts the session handles.
pub trait SessionFactory {
    /// Start the handle of one session request. The factory gives the
    /// handle its identity. A failed start comes later as an error response
    /// and an end message of the handle.
    /// Args:
    ///  - `context`: Fixed settings of all handles
    ///  - `cwd`: Session dir of the session request on the host
    ///  - `first`: The session request. The agent gets it after its
    ///    handshake.
    ///
    /// Returns:
    ///   The identity and the new handle.
    fn create(
        &self,
        context: &Rc<SessionContext>,
        cwd: String,
        first: Value,
    ) -> (HandleId, Box<dyn SessionHandle>);

    /// Get the host session dir of a session that a list response of this
    /// factory named. A delete of the session removes it.
    /// Returns:
    ///   The session dir, or `None` if no recent list named the session.
    fn session_dir(&self, sid: &str) -> Option<String>;
}

/// Maximum number of parts that wait for the client output. A full queue
/// stops the proxy until the client reads.
const WRITER_QUEUE: usize = 4;

/// Maximum number of client lines that wait for the proxy. A full queue
/// stops the read of the client input.
const READER_QUEUE: usize = 16;

/// Maximum number of ACP sessions of one handle. The agent makes new
/// sessions, for example for sub agents.
const MAX_HANDLE_SESSIONS: usize = 64;

/// Maximum number of handle messages that wait for the proxy.
const HANDLE_MESSAGES: usize = 16;

/// Run the proxy between a client and its session handles.
/// Args:
///  - `factory`: Starts a handle for each session request
///  - `config`: Proxy settings
///  - `input`: Client messages, one per line
///  - `output`: Destination of the messages to the client
///
/// Returns:
///   The first exit code that is not 0 of the handles, else 0. 128 + the
///   signal number if a signal came when no live handle had sent
///   [`HandleMessage::Ready`].
pub async fn run<R, W>(
    factory: Box<dyn SessionFactory>,
    config: &Config,
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
        factory,
        messages,
        context: RefCell::default(),
        config,
        out,
        kill: CancellationToken::new(),
        ended: CancellationToken::new(),
        stopping: RefCell::default(),
        stopped: Notify::new(),
        closing: Cell::new(false),
        started: Cell::new(false),
        handles: RefCell::default(),
        sessions: RefCell::default(),
        client_requests: RefCell::default(),
        ready: RefCell::default(),
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
    factory: Box<dyn SessionFactory>,
    /// Destination of the handle messages, for the handle settings.
    messages: mpsc::Sender<(HandleId, HandleMessage)>,
    /// Settings of the handles, after the client handshake.
    context: RefCell<Option<Rc<SessionContext>>>,
    config: &'a Config,
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
    /// A handle started.
    started: Cell<bool>,
    handles: RefCell<HashMap<HandleId, Box<dyn SessionHandle>>>,
    /// Handle of each ACP session.
    sessions: RefCell<HashMap<SessionId, HandleId>>,
    /// Handle of each open client request, by request id.
    client_requests: RefCell<HashMap<RequestId, HandleId>>,
    /// The handles that sent [`HandleMessage::Ready`] and did not end.
    ready: RefCell<HashSet<HandleId>>,
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
                    if self.ready.borrow().is_empty() {
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

    /// Start the stop of the handle `hid`. Its sessions and open client
    /// requests stay until the handle ends. The agent can still answer
    /// them, but the handle takes no new client messages.
    fn stop_handle(&self, hid: HandleId) {
        let Some(proxy) = self.handles.borrow_mut().remove(&hid) else {
            return;
        };
        tracing::info!(handle = hid, "stop the session handle");
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
                let Some(hid) = rpc::agent_request_handle(req_id) else {
                    tracing::debug!(id = %req_id, "drop client response without a handle");
                    return;
                };
                self.deliver(hid, msg);
            }
            Some(Kind::CancelRequest { req_id }) => {
                // A cancel without a request id has no target. A cancel of
                // an agent request goes to the handle of the agent.
                let hid = req_id.and_then(|req_id| {
                    let client = self.client_requests.borrow().get(&key(req_id)).copied();
                    client.or_else(|| rpc::agent_request_handle(req_id))
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
            // A load or resume gets the session when its handle accepts
            // it (see `HandleMessage::Opening`).
            "session/new" | "session/fork" | "session/load" | "session/resume" => {
                self.handle_session_new(req_id, method, msg).await;
            }
            "session/list" => self.handle_session_list(req_id, method, msg).await,
            "authenticate" | "logout" => self.reply_not_supported(&req_id, method).await,
            m if m.starts_with("providers/") => self.reply_not_supported(&req_id, method).await,
            _ => self.delegate_to_session(req_id, method, msg).await,
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
        let known = self.context.borrow().as_ref().map(|c| c.offered.clone());
        let result = known.unwrap_or_else(|| self.first_handshake(msg));
        self.reply(req_id, Ok(result)).await;
    }

    /// Filter the client params of the first `initialize` request `msg` for
    /// the agents, and make the handle settings with them.
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
        *self.context.borrow_mut() = Some(Rc::new(SessionContext {
            init_params: params,
            offered: result.clone(),
            messages: self.messages.clone(),
            kill: self.kill.clone(),
        }));
        result
    }

    /// Handle a session request (new, fork, load or resume): start a
    /// handle for the session, or refuse the request.
    async fn handle_session_new(&self, req_id: Value, method: &str, msg: Value) {
        let cwd = msg.pointer("/params/cwd").and_then(Value::as_str);
        let cwd = cwd.map(str::to_string);
        if let Err(error) = self.start_handle(&req_id, cwd, msg) {
            tracing::info!(method, "refuse session request: {}", error.1);
            self.reply(&req_id, Err(error)).await;
        }
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
    async fn delegate_to_session(&self, req_id: Value, method: &str, msg: Value) {
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
            if let Some(cwd) = self.factory.session_dir(sid) {
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
        self.client_requests.borrow_mut().insert(key(&req_id), hid);
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
        if self.closing.get() {
            return Err((rpc::INTERNAL_ERROR, messages::acp::SHUTTING_DOWN.into()));
        }
        let Some(context) = self.context.borrow().clone() else {
            return Err((rpc::INVALID_REQUEST, messages::acp::NO_HANDSHAKE.into()));
        };
        let Some(cwd) = cwd else {
            return Err((rpc::INVALID_PARAMS, messages::acp::NO_SESSION_DIR.into()));
        };
        if !cwd.starts_with('/') {
            return Err((rpc::INVALID_PARAMS, messages::acp::cwd_not_absolute(&cwd)));
        }
        tracing::info!(cwd, "start a session handle");
        self.started.set(true);
        let (hid, proxy) = self.factory.create(&context, cwd, msg);
        self.client_requests.borrow_mut().insert(key(req_id), hid);
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
            HandleMessage::Ready => {
                self.ready.borrow_mut().insert(hid);
            }
            HandleMessage::Opening { session } => self.on_opening(hid, session),
            HandleMessage::Idle => {
                self.ready.borrow_mut().remove(&hid);
            }
            HandleMessage::Ended => self.on_handle_end(hid),
        }
    }

    /// Send one agent message of the handle `hid` to the client. Rename the
    /// id of an agent request, and keep the handle to its own sessions.
    async fn on_handle_message(&self, hid: HandleId, mut msg: Value, claim: Option<Claim>) {
        let mut refused = false;
        match rpc::kind(&msg) {
            Some(Kind::Response { id: req_id }) => {
                let req_id = req_id.clone();
                let mut requests = self.client_requests.borrow_mut();
                if requests.get(&key(&req_id)) != Some(&hid) {
                    tracing::debug!(handle = hid, id = %req_id, "drop agent response without a request");
                    return;
                }
                requests.remove(&key(&req_id));
                drop(requests);
                if let Some(claim) = claim
                    && !self.claim(hid, &claim)
                {
                    let message = messages::acp::session_id_conflict(&claim.session);
                    // The session id comes from the agent. The debug form
                    // escapes its control characters.
                    tracing::warn!(handle = hid, "refuse session: {message:?}");
                    msg = rpc::error_response(&req_id, rpc::INTERNAL_ERROR, &message);
                    refused = true;
                }
            }
            Some(kind) if !self.owns(hid, &msg) => {
                tracing::debug!(handle = hid, "drop agent message for another session");
                // The agent must not wait for an answer that never comes.
                if let Kind::Request { id: req_id, .. } = kind {
                    let text = messages::acp::OTHER_SESSION_REQUEST;
                    self.deliver(hid, rpc::error_response(req_id, rpc::INVALID_PARAMS, text));
                }
                return;
            }
            Some(_) => {}
            None => {
                tracing::debug!(handle = hid, "drop agent message that is not JSON-RPC");
                return;
            }
        }
        let _ = self.out.send(to_line(&msg)).await;
        // The handle has no session.
        if refused {
            self.stop_handle(hid);
        }
    }

    /// Give the session `sid` to `hid` at the start of its load, and stop
    /// the old handle of the session. The last output of the old handle
    /// then cannot reach the session.
    fn on_opening(&self, hid: HandleId, sid: SessionId) {
        if !self.handles.borrow().contains_key(&hid) {
            return;
        }
        let old = self.sessions.borrow_mut().insert(sid, hid);
        if let Some(old) = old.filter(|old| *old != hid) {
            self.stop_handle(old);
        }
    }

    /// Give the session of `claim` to `hid`. A handle that stops gets no
    /// session. A load or resume has the session already from its start.
    /// Returns:
    ///   `false` if another handle has the session.
    fn claim(&self, hid: HandleId, claim: &Claim) -> bool {
        if !self.handles.borrow().contains_key(&hid) {
            return true;
        }
        let mut sessions = self.sessions.borrow_mut();
        if sessions
            .get(&claim.session)
            .is_some_and(|owner| *owner != hid)
        {
            return false;
        }
        sessions.insert(claim.session.clone(), hid);
        true
    }

    /// Check that the session of the agent message `msg` is a session of
    /// `hid`. A new session id becomes a session of `hid`, for example
    /// the session of a sub agent, up to [`MAX_HANDLE_SESSIONS`] and with
    /// at most [`rpc::MAX_ID_BYTES`]. A handle that stops keeps its
    /// sessions until it ends, and gets no new session.
    // A load gives the session to its new handle when the handle accepts
    // the load. The last output of the old handle then cannot take it back.
    fn owns(&self, hid: HandleId, msg: &Value) -> bool {
        let sid = match msg.pointer("/params/sessionId") {
            None => return true,
            Some(Value::String(sid)) => sid,
            // A session id that is not a string names no session.
            Some(_) => return false,
        };
        let mut sessions = self.sessions.borrow_mut();
        if let Some(owner) = sessions.get(sid) {
            return *owner == hid;
        }
        if !self.handles.borrow().contains_key(&hid) {
            return false;
        }
        // The agent is not trusted. Its new sessions must not fill the
        // memory.
        let count = sessions.values().filter(|owner| **owner == hid).count();
        if sid.len() > rpc::MAX_ID_BYTES || count >= MAX_HANDLE_SESSIONS {
            tracing::debug!(handle = hid, count, "refuse new agent session");
            return false;
        }
        sessions.insert(sid.clone(), hid);
        true
    }

    /// Forget a handle that ended. The reaper gets its exit code.
    fn on_handle_end(&self, hid: HandleId) {
        self.client_requests
            .borrow_mut()
            .retain(|_, owner| *owner != hid);
        self.ready.borrow_mut().remove(&hid);
        self.stop_handle(hid);
        // A load can have given a session of the handle to a new handle.
        self.sessions.borrow_mut().retain(|_, owner| *owner != hid);
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
