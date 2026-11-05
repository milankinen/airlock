//! Agents of the sessions in their sandboxes.
//!
//! Runs one agent process for each sandbox. All sessions with a session
//! dir in the same sandbox share that process. The proxy sees each session
//! as its own session handle, with its own identity. A session in a
//! sandbox that has no running agent launches a new one. When the last
//! session of an agent ends, the agent stops. When the agent exits, all its
//! sessions end.

pub(super) mod launcher;
mod session;
mod shared;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use futures::future::LocalBoxFuture;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use self::shared::Link;
use super::proxy::{
    HandleId, HandleMessage, SessionContext, SessionFactory, SessionFactoryInit, SessionHandle,
};
use super::{AgentProcessLauncher, Location, NoSandbox, rpc};

/// Settings of the agents.
pub struct AgentConfig {
    /// Agent command and its arguments.
    pub argv: Vec<String>,
    /// Agent command as the user gave it, for messages.
    pub name: String,
    /// Remove the client features that let the agent act on the host.
    pub restrict: bool,
    /// Bridge the editor MCP servers into the sandbox. If not set, the
    /// agent gets no editor MCP servers.
    pub proxy_mcp_servers: bool,
    /// Maximum time to register the editor MCP servers of one session
    /// request.
    pub mcp_register_timeout: Duration,
    /// Maximum time from the agent start to the end of the handshake.
    pub handshake_timeout: Duration,
    /// Time that an agent has to exit after the proxy closes its input,
    /// and again after the kill.
    pub shutdown_grace: Duration,
    /// Maximum length of one agent output line.
    pub max_line_bytes: usize,
}

/// Make the factory of the agents that `launcher` launches.
/// Args:
///  - `launcher`: Launches the agent processes in their sandboxes
///  - `config`: Agent settings
///  - `stderr`: Destination of the agent stderr
///
/// Returns:
///   The factory, and the number of agent stderr bytes that were dropped.
pub(super) fn factory<S: AgentProcessLauncher + 'static>(
    launcher: S,
    config: AgentConfig,
    stderr: mpsc::Sender<Vec<u8>>,
) -> (Box<dyn SessionFactoryInit>, Rc<Cell<u64>>) {
    let dropped = Rc::new(Cell::new(0));
    let init = SandboxAgentsInit {
        launcher,
        config: Rc::new(config),
        stderr: Stderr {
            tx: stderr,
            dropped: dropped.clone(),
        },
    };
    (Box::new(init), dropped)
}

/// Agent stderr to the host stderr.
struct Stderr {
    tx: mpsc::Sender<Vec<u8>>,
    /// Number of agent stderr bytes that were dropped.
    dropped: Rc<Cell<u64>>,
}

/// Makes the [`SandboxAgents`] at the client handshake.
struct SandboxAgentsInit<S> {
    launcher: S,
    config: Rc<AgentConfig>,
    stderr: Stderr,
}

impl<S: AgentProcessLauncher + 'static> SessionFactoryInit for SandboxAgentsInit<S> {
    fn init(self: Box<Self>, context: SessionContext) -> Rc<dyn SessionFactory> {
        let notices = context
            .init_params
            .pointer("/clientCapabilities/session/notices")
            .is_some();
        let pool = Pool {
            launcher: self.launcher,
            config: self.config,
            stderr: self.stderr,
            init_params: context.init_params,
            offered: context.offered,
            messages: context.messages,
            kill: context.kill,
            notices,
            next_id: Cell::new(0),
            agents: RefCell::default(),
            listed: RefCell::default(),
        };
        Rc::new(SandboxAgents {
            pool: Rc::new(pool),
        })
    }
}

/// Settings and running agents of all sessions.
struct Pool<S> {
    launcher: S,
    config: Rc<AgentConfig>,
    stderr: Stderr,
    /// Client `initialize` params after the filter.
    init_params: Value,
    /// `initialize` result that the proxy gave to the client.
    offered: Value,
    /// Destination of the session messages.
    messages: mpsc::Sender<(HandleId, HandleMessage)>,
    /// Kills the agents at once in their stop.
    kill: CancellationToken,
    /// The client shows session notices.
    notices: bool,
    /// Identity of the next session.
    next_id: Cell<HandleId>,
    /// The agent of each sandbox.
    agents: RefCell<HashMap<String, Rc<Link>>>,
    /// Host session dirs of the sessions in the latest session lists.
    listed: RefCell<Listed>,
}

impl<S: AgentProcessLauncher + 'static> Pool<S> {
    /// Get the agent of `location` that takes new sessions, or launch a
    /// new one there.
    /// Args:
    ///  - `cwd`: Host session dir, for the launch
    fn agent(self: &Rc<Self>, location: &Location, cwd: &str) -> Rc<Link> {
        let mut agents = self.agents.borrow_mut();
        if let Some(link) = agents.get(&location.sandbox)
            && link.accepting()
        {
            return link.clone();
        }
        let link = shared::spawn(self.clone(), location.sandbox.clone(), cwd.to_string());
        agents.insert(location.sandbox.clone(), link.clone());
        link
    }

    /// Forget the agent `link` of `sandbox` when it ended.
    fn forget(&self, sandbox: &str, link: &Rc<Link>) {
        let mut agents = self.agents.borrow_mut();
        if agents.get(sandbox).is_some_and(|l| Rc::ptr_eq(l, link)) {
            agents.remove(sandbox);
        }
    }
}

/// Gives each session its own handle on the agent of its sandbox.
struct SandboxAgents<S> {
    pool: Rc<Pool<S>>,
}

impl<S: AgentProcessLauncher + 'static> SessionFactory for SandboxAgents<S> {
    fn create(&self, cwd: String, first: Value) -> (HandleId, Box<dyn SessionHandle>) {
        let hid = self.pool.next_id.get();
        self.pool.next_id.set(hid + 1);
        tracing::info!(handle = hid, cwd, "new session");
        let state = Rc::new(RefCell::new(HandleState::Pending(Vec::new())));
        tokio::task::spawn_local(attach(self.pool.clone(), hid, cwd, first, state.clone()));
        (hid, Box::new(Handle { hid, state }))
    }

    fn session_dir(&self, sid: &str) -> Option<String> {
        self.pool.listed.borrow().dirs.get(sid).cloned()
    }
}

/// Maximum number of listed sessions that the factory keeps.
const MAX_LISTED: usize = 4096;

/// Host session dirs of the listed sessions. When the map is full, a new
/// session removes the oldest one.
#[derive(Default)]
pub(super) struct Listed {
    dirs: HashMap<String, String>,
    /// The session ids, the oldest first.
    order: VecDeque<String>,
}

impl Listed {
    /// Keep the sessions of the OK response `msg` to the one-shot request
    /// `method`. Forget the session of a delete that completed.
    /// Args:
    ///  - `cwd`: Host session dir of the request
    ///  - `sid`: Session id of the request, if it has one
    // The sessions get the dir of the list request, not the dir of their
    // row. A row dir can be outside the sandbox, for example a git
    // worktree. The agent that knows the session is in the sandbox of the
    // list request.
    pub(super) fn record(&mut self, method: &str, cwd: &str, sid: Option<&str>, msg: &Value) {
        if msg.get("result").is_none() {
            return;
        }
        match method {
            "session/list" => {
                let rows = msg.pointer("/result/sessions").and_then(Value::as_array);
                let sids = rows
                    .into_iter()
                    .flatten()
                    .filter_map(|row| row.get("sessionId").and_then(Value::as_str));
                for sid in sids {
                    self.insert(sid, cwd);
                }
            }
            "session/delete" => {
                if let Some(sid) = sid {
                    self.remove(sid);
                }
            }
            _ => {}
        }
    }

    /// Keep the session `sid` with the host session dir `cwd`, as the
    /// newest session.
    fn insert(&mut self, sid: &str, cwd: &str) {
        if self.dirs.insert(sid.to_string(), cwd.to_string()).is_some() {
            self.order.retain(|s| s != sid);
        }
        self.order.push_back(sid.to_string());
        while self.order.len() > MAX_LISTED {
            if let Some(oldest) = self.order.pop_front() {
                self.dirs.remove(&oldest);
            }
        }
    }

    /// Forget the session `sid`.
    fn remove(&mut self, sid: &str) {
        if self.dirs.remove(sid).is_some() {
            self.order.retain(|s| s != sid);
        }
    }
}

/// Maximum number of request params characters in the start failure log.
const MAX_LOGGED_PARAMS: usize = 500;

/// Find the sandbox of the session request `first` and attach the session
/// `hid` to the agent there.
async fn attach<S: AgentProcessLauncher + 'static>(
    pool: Rc<Pool<S>>,
    hid: HandleId,
    cwd: String,
    first: Value,
    state: Rc<RefCell<HandleState>>,
) {
    let located = pool.launcher.locate(&cwd).await;
    if matches!(*state.borrow(), HandleState::Ended) {
        return;
    }
    let location = match located {
        Ok(location) => location,
        Err(e) => {
            let method = first.get("method").and_then(Value::as_str).unwrap_or("-");
            let req_id = first.get("id").unwrap_or(&Value::Null);
            // Editors list the sessions of old dirs, for example removed
            // git worktrees. These dirs have no sandbox, thus they have
            // no sessions. An error makes the editor show a failure.
            if method == "session/list" && e.downcast_ref::<NoSandbox>().is_some() {
                tracing::debug!(handle = hid, "empty session list: {e:#}");
                let msg = rpc::result_response(req_id, &json!({ "sessions": [] }));
                end_early(&pool, hid, msg, &state).await;
                return;
            }
            let message = format!("{e:#}");
            // Log the first request to show which client call wants the
            // sandbox. Cut long params, for example prompt text.
            let mut params = first
                .get("params")
                .map(Value::to_string)
                .unwrap_or_default();
            if let Some((cut, _)) = params.char_indices().nth(MAX_LOGGED_PARAMS) {
                params.truncate(cut);
                params.push('…');
            }
            tracing::warn!(
                handle = hid,
                method,
                params,
                "agent start failed: {message}"
            );
            let msg = rpc::error_response(req_id, rpc::INTERNAL_ERROR, &message);
            end_early(&pool, hid, msg, &state).await;
            return;
        }
    };
    let link = pool.agent(&location, &cwd);
    let state = std::mem::replace(
        &mut *state.borrow_mut(),
        HandleState::Attached(link.clone()),
    );
    if let HandleState::Pending(queued) = state {
        link.attach(hid, location, cwd, first, queued);
    }
}

/// End the session `hid` before it attaches to an agent. Send the
/// response `msg` to the first request.
async fn end_early<S>(pool: &Pool<S>, hid: HandleId, msg: Value, state: &RefCell<HandleState>) {
    *state.borrow_mut() = HandleState::Ended;
    let claim = None;
    let _ = pool
        .messages
        .send((hid, HandleMessage::Client { msg, claim }))
        .await;
    let _ = pool.messages.send((hid, HandleMessage::Ended)).await;
}

/// State of a session handle.
enum HandleState {
    /// The sandbox lookup runs. The client messages wait here.
    Pending(Vec<Value>),
    /// The session is in the agent of its sandbox.
    Attached(Rc<Link>),
    /// The session ended.
    Ended,
}

/// Proxy side of one session.
struct Handle {
    hid: HandleId,
    state: Rc<RefCell<HandleState>>,
}

impl SessionHandle for Handle {
    fn send(&self, msg: Value) -> bool {
        match &mut *self.state.borrow_mut() {
            HandleState::Pending(queued) => {
                queued.push(msg);
                true
            }
            HandleState::Attached(link) => link.send(self.hid, msg),
            HandleState::Ended => false,
        }
    }

    fn shutdown(self: Box<Self>) -> LocalBoxFuture<'static, i32> {
        match self.state.replace(HandleState::Ended) {
            HandleState::Attached(link) => link.detach(self.hid),
            HandleState::Pending(_) | HandleState::Ended => Box::pin(std::future::ready(0)),
        }
    }
}
