//! Agents of the sessions in their sandboxes.
//!
//! Each session runs its own agent process in the sandbox of its session
//! dir, also a one-shot request such as a session list. The proxy sees
//! each session as its own session handle, with its own identity. The
//! handle gives the client messages to the session in order. Its stop
//! stops the agent of the session after the earlier messages.
//!
//! Also keeps the host session dirs of the sessions in the latest session
//! lists. A request for such a session can then start in the correct
//! sandbox.

pub(super) mod launcher;
mod output;
mod process;
mod session;

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use futures::future::LocalBoxFuture;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::Instrument;

use super::AgentProcessLauncher;
use super::proxy::{
    HandleId, HandleMessage, SessionContext, SessionFactory, SessionFactoryInit, SessionHandle,
};

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
#[derive(Clone)]
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
            listed: RefCell::default(),
        };
        Rc::new(SandboxAgents {
            pool: Rc::new(pool),
        })
    }
}

/// Settings of all sessions.
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
    /// Host session dirs of the sessions in the latest session lists.
    listed: RefCell<Listed>,
}

/// Starts a session task with its own agent for each session.
struct SandboxAgents<S> {
    pool: Rc<Pool<S>>,
}

impl<S: AgentProcessLauncher + 'static> SessionFactory for SandboxAgents<S> {
    fn create(&self, cwd: String, first: Value) -> (HandleId, Box<dyn SessionHandle>) {
        let hid = self.pool.next_id.get();
        self.pool.next_id.set(hid + 1);
        tracing::info!(handle = hid, cwd, "new session");
        let (tx, inbox) = mpsc::unbounded_channel();
        let run = session::run_session(self.pool.clone(), hid, cwd, first, inbox);
        // All logs of the session, also of its agent and sandbox start,
        // show the handle.
        let span = tracing::info_span!("session", handle = hid);
        let task = tokio::task::spawn_local(run.instrument(span));
        (hid, Box::new(Handle { tx, task }))
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

/// Proxy side of one session.
struct Handle {
    /// Client messages for the session task. The drop stops the task.
    tx: mpsc::UnboundedSender<Value>,
    /// The session task. It gives the exit code of the session.
    task: JoinHandle<i32>,
}

impl SessionHandle for Handle {
    fn send(&self, msg: Value) -> bool {
        self.tx.send(msg).is_ok()
    }

    fn shutdown(self: Box<Self>) -> LocalBoxFuture<'static, i32> {
        let Self { tx, task } = *self;
        // The task stops the session after the messages that came before.
        drop(tx);
        // A task that panicked is a failure.
        Box::pin(async move { task.await.unwrap_or(1) })
    }
}
