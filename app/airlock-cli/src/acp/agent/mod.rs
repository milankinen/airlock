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
use std::rc::Rc;

use futures::future::LocalBoxFuture;
use quick_cache::unsync::Cache;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::Instrument;

use super::proxy::{HandleId, SessionContext, SessionFactory, SessionHandle};
use super::{AgentProcessLauncher, Config, rpc};

/// Make the factory of the agents that `launcher` launches.
/// Args:
///  - `launcher`: Launches the agent processes in their sandboxes
///  - `config`: Agent settings
///  - `stderr`: Destination of the agent stderr
pub(super) fn factory<S: AgentProcessLauncher + 'static>(
    launcher: S,
    config: Rc<Config>,
    stderr: Stderr,
) -> Box<dyn SessionFactory> {
    let pool = Pool {
        launcher,
        config,
        stderr,
        next_id: Cell::new(0),
        listed: RefCell::default(),
    };
    Box::new(SandboxAgents {
        pool: Rc::new(pool),
    })
}

/// Agent stderr to the host stderr.
type Stderr = mpsc::Sender<Vec<u8>>;

/// Settings and state of all sessions.
struct Pool<S> {
    launcher: S,
    config: Rc<Config>,
    stderr: Stderr,
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
    fn create(
        &self,
        context: &Rc<SessionContext>,
        cwd: String,
        first: Value,
    ) -> (HandleId, Box<dyn SessionHandle>) {
        let hid = self.pool.next_id.get();
        self.pool.next_id.set(hid + 1);
        tracing::info!(handle = hid, cwd, "new session");
        let (tx, inbox) = mpsc::unbounded_channel();
        let pool = self.pool.clone();
        let run = session::run_session(pool, context.clone(), hid, cwd, first, inbox);
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

/// Maximum number of listed sessions that the factory keeps, and of the
/// rows of one list that it reads.
const MAX_LISTED: usize = 4096;

/// Host session dirs of the listed sessions. When the cache is full, a new
/// session takes the place of an old one. The agent is not trusted, and a
/// list can be long.
pub(super) struct Listed {
    dirs: Cache<String, String>,
}

impl Default for Listed {
    fn default() -> Self {
        Self {
            dirs: Cache::new(MAX_LISTED),
        }
    }
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
                    .take(MAX_LISTED)
                    .filter_map(|row| row.get("sessionId").and_then(Value::as_str))
                    .filter(|sid| sid.len() <= rpc::MAX_ID_BYTES);
                for sid in sids {
                    self.dirs.insert(sid.to_string(), cwd.to_string());
                }
            }
            "session/delete" => {
                if let Some(sid) = sid {
                    self.dirs.remove(sid);
                }
            }
            _ => {}
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

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Test that a session list with too many rows or too long ids does not
    /// grow the cache without limit, and that a delete forgets its
    /// session. The agent is not trusted.
    ///   1. Record a list with a too long id and more than the maximum rows
    ///   2. Check that the cache has at most the maximum rows, without the
    ///      long id
    ///   3. Record a list with a new id and a delete of that id, and check
    ///      that the id is gone
    #[test]
    fn listed_sessions_have_a_limit() {
        let long = "x".repeat(rpc::MAX_ID_BYTES + 1);
        let mut rows = vec![json!({ "sessionId": long })];
        rows.extend((0..MAX_LISTED + 10).map(|i| json!({ "sessionId": format!("s{i}") })));
        let list = |rows: Vec<Value>| json!({ "result": { "sessions": rows } });
        let mut listed = Listed::default();
        listed.record("session/list", "/p", None, &list(rows));
        assert!(listed.dirs.len() <= MAX_LISTED);
        assert!(listed.dirs.get(&long).is_none());

        let new = list(vec![json!({ "sessionId": "new" })]);
        listed.record("session/list", "/p", None, &new);
        assert_eq!(listed.dirs.get("new").map(String::as_str), Some("/p"));
        let deleted = json!({ "result": {} });
        listed.record("session/delete", "/p", Some("new"), &deleted);
        assert!(listed.dirs.get("new").is_none());
        assert!(listed.dirs.len() <= MAX_LISTED);
    }
}
