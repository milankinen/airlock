//! Host MCP servers.
//!
//! Gives the guest access to MCP servers that run on the host. The servers
//! come from two sources: the project config enables them at the sandbox
//! start, and `airlock acp` adds them at run time for the editor sessions.
//! The guest serves each server as an HTTP endpoint. For a stdio server,
//! the host runs the server program and relays the JSON-RPC messages
//! between the guest and the program, one message for each line. For a
//! remote server, the host relays the messages to the server URL over
//! HTTP.
//!
//! Each guest MCP session gets its own server session. The session stops
//! when the guest releases it, and the guest learns when the session
//! stops. The host also stops sessions that are idle for too long, and the
//! session with the oldest activity when a server has too many sessions.
//! The owner of the servers can close them all at a time. Then all live
//! sessions stop and the guest cannot open new ones. The guest can start
//! only the servers that the host gives.
//!
//! The environment values of the servers resolve on the host. Their
//! values never go to the guest.

mod remote;
mod session;

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::rc::{Rc, Weak};
use std::time::Duration;

use airlock_common::supervisor_capnp::{mcp_host, mcp_session};
use tokio::sync::Notify;

pub(crate) use self::remote::{RemoteKind, RemoteSpec, deletes_done};
use crate::config::config_values::McpServer;
use crate::mcp::session::SessionState;
use crate::messages;
use crate::project::Project;
use crate::util::VerifiedPath;
use crate::vault::Vault;

/// One MCP server that the guest can start.
#[derive(Debug, Clone)]
pub(crate) struct ServerSpec {
    /// Name of the server in its guest route, for example from
    /// `[mcp.<name>]`.
    pub(crate) name: String,
    /// How the host reaches the server.
    pub(crate) transport: Transport,
    /// Idle time after which the host stops a session. `None`: no limit.
    pub(crate) timeout: Option<Duration>,
    /// Maximum number of live sessions, 1 or more.
    pub(crate) max_sessions: usize,
}

/// How the host reaches one MCP server.
#[derive(Debug, Clone)]
pub(crate) enum Transport {
    /// Host program that reads JSON-RPC messages from its stdin and writes
    /// them to its stdout, one message for each line.
    Stdio {
        /// Program to run.
        command: String,
        /// Arguments of the program.
        args: Vec<String>,
        /// Resolved environment values, on top of the host environment.
        env: Vec<(String, String)>,
    },
    /// MCP server at a URL. The host relays the messages over HTTP.
    Remote(RemoteSpec),
}

/// Live sessions by server name.
type Sessions = BTreeMap<String, Vec<Rc<SessionState>>>;

/// State that the host and its stopper share.
#[derive(Default)]
struct Shared {
    /// Live sessions of each server, to apply `max_sessions`. A session
    /// removes itself when it ends.
    sessions: RefCell<Sessions>,
    /// Set when the owner closed the host. Then `open` fails.
    closed: Cell<bool>,
    /// Signal when the last live session ended.
    idle: Notify,
}

/// Cap'n Proto `McpHost` server that starts the host MCP servers for the
/// guest.
///
/// Each call checks the server name again here. It does not trust
/// anything in the sandbox.
pub struct McpHostImpl {
    /// Servers by name.
    servers: BTreeMap<String, ServerSpec>,
    /// Working directory of the server processes.
    cwd: VerifiedPath,
    /// Live sessions and the closed flag, shared with the stoppers.
    shared: Rc<Shared>,
}

/// Handle that closes an MCP host. It stays valid after the host moved
/// into an RPC capability.
pub(crate) struct HostStopper {
    /// State of the host.
    shared: Rc<Shared>,
}

impl HostStopper {
    /// Close the host. Later `open` calls fail. Each live session stops,
    /// and its guest gets `reason` as the close reason.
    pub(crate) fn close(&self, reason: &str) {
        self.shared.closed.set(true);
        // Collect first: a stop must not run while the registry is
        // borrowed.
        let live: Vec<Rc<SessionState>> = self
            .shared
            .sessions
            .borrow()
            .values()
            .flatten()
            .cloned()
            .collect();
        for state in live {
            state.stop_with(reason.to_string());
        }
    }

    /// Wait until the host has no live sessions. After a close, a remote
    /// session ends after it started its end request to the server.
    pub(crate) async fn stopped(&self) {
        loop {
            let idle = self.shared.idle.notified();
            tokio::pin!(idle);
            // Register before the check, so that a signal between the
            // check and the wait is not lost.
            idle.as_mut().enable();
            if self.shared.sessions.borrow().is_empty() {
                return;
            }
            idle.await;
        }
    }
}

/// Make the MCP capability for the `[mcp]` config of `project`. The server
/// processes run in the project directory.
/// Returns:
///   Capability, or `None` if the config enables no server. Error if a
///   `${VAR}` template in a server env does not resolve.
pub fn for_project(project: &Project) -> anyhow::Result<Option<McpHostImpl>> {
    resolve(
        &project.config.mcp,
        &project.context.vault,
        &project.host_cwd,
    )
}

/// Make the MCP capability for the `[mcp]` entries `config`.
/// Args:
///  - `config`: The `[mcp]` entries of the project config
///  - `vault`: Vault for the `${VAR}` templates of the server env
///  - `cwd`: Working directory of the server processes
///
/// Returns:
///   Capability, or `None` if no entry is enabled. Error if a `${VAR}`
///   template in a server env does not resolve.
pub(crate) fn resolve(
    config: &BTreeMap<String, McpServer>,
    vault: &Vault,
    cwd: &VerifiedPath,
) -> anyhow::Result<Option<McpHostImpl>> {
    let servers = config
        .iter()
        .filter(|(_, s)| s.enabled)
        .map(|(name, s)| {
            let env = s
                .env
                .iter()
                .map(|(key, template)| {
                    let value = vault
                        .subst(template)
                        .map_err(|e| anyhow::anyhow!(messages::mcp::env_failed(name, key, e)))?;
                    Ok((key.clone(), value))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            Ok(ServerSpec {
                name: name.clone(),
                transport: Transport::Stdio {
                    command: s.command.clone(),
                    args: s.args.clone(),
                    env,
                },
                timeout: (s.timeout > 0).then(|| Duration::from_secs(s.timeout.into())),
                max_sessions: usize::try_from(s.max_sessions).unwrap_or(usize::MAX).max(1),
            })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    if servers.is_empty() {
        return Ok(None);
    }
    Ok(Some(McpHostImpl::new(servers, cwd.clone())))
}

impl McpHostImpl {
    /// Make a host that serves the given servers.
    /// Args:
    ///  - `servers`: Servers that the guest can start. For two servers with
    ///    the same name, the last one stays.
    ///  - `cwd`: Working directory of the stdio server processes
    pub(crate) fn new(servers: Vec<ServerSpec>, cwd: VerifiedPath) -> Self {
        Self {
            servers: servers.into_iter().map(|s| (s.name.clone(), s)).collect(),
            cwd,
            shared: Rc::default(),
        }
    }

    /// Names of the servers, in name order.
    pub(crate) fn names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
    }

    /// Make a handle that closes this host and stops its sessions.
    pub(crate) fn stopper(&self) -> HostStopper {
        HostStopper {
            shared: self.shared.clone(),
        }
    }
}

impl mcp_host::Server for McpHostImpl {
    async fn open(
        self: Rc<Self>,
        params: mcp_host::OpenParams,
        mut results: mcp_host::OpenResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let name = params.get_name()?.to_str()?;
        let spec = self
            .servers
            .get(name)
            .ok_or_else(|| capnp::Error::failed(messages::mcp::unknown_server(name)))?;
        if self.shared.closed.get() {
            return Err(capnp::Error::failed(messages::mcp::host_closed(name)));
        }
        let sink = params.get_sink()?;
        // A remote server can need a connection before the session starts.
        // After it, check the closed flag again.
        let remote = match &spec.transport {
            Transport::Remote(remote) => {
                let connected = remote::connect(&spec.name, remote).await?;
                if self.shared.closed.get() {
                    return Err(capnp::Error::failed(messages::mcp::host_closed(name)));
                }
                Some(connected)
            }
            Transport::Stdio { .. } => None,
        };
        // The session removes itself from the registry when it ends. The
        // registry does not keep it.
        let registry = Rc::downgrade(&self.shared);
        let ended: Rc<RefCell<Option<Rc<SessionState>>>> = Rc::default();
        let on_end = {
            let ended = ended.clone();
            let name = spec.name.clone();
            move || forget(&registry, &name, ended.take())
        };
        // No await from the last closed check to the registration of the
        // session. Thus a close cannot miss a session that opens now.
        let (client, state): (mcp_session::Client, _) = match (&spec.transport, remote) {
            (Transport::Stdio { command, args, env }, _) => {
                let program = session::StdioProgram { command, args, env };
                let session = session::start(spec, &program, &self.cwd, sink, on_end)?;
                let state = session.state();
                (capnp_rpc::new_client(session), state)
            }
            (Transport::Remote(_), Some(connected)) => {
                let session = remote::start(spec, connected, sink, on_end);
                let state = session.state();
                (capnp_rpc::new_client(session), state)
            }
            (Transport::Remote(_), None) => unreachable!("remote server connected above"),
        };
        *ended.borrow_mut() = Some(state.clone());
        // Make room only after the new session started. Thus a failed
        // start does not stop a working session.
        self.make_room(spec, state);
        results.get().set_session(client);
        Ok(())
    }
}

impl McpHostImpl {
    /// Add the new session `added` of `spec`. If the server then has more
    /// than `max_sessions` running sessions, stop the others with the oldest
    /// activity. A busy session can also stop: else a guest could keep the
    /// limit full.
    fn make_room(&self, spec: &ServerSpec, added: Rc<SessionState>) {
        let mut sessions = self.shared.sessions.borrow_mut();
        let live = sessions.entry(spec.name.clone()).or_default();
        let added_ptr = Rc::as_ptr(&added);
        live.push(added);
        // A session that got the stop signal ends soon and removes itself.
        // It does not count, so it cannot cause the stop of a working one.
        while live.iter().filter(|s| !s.is_stopped()).count() > spec.max_sessions {
            let Some(oldest) = live
                .iter()
                .enumerate()
                .filter(|(_, s)| Rc::as_ptr(s) != added_ptr && !s.is_stopped())
                .min_by_key(|(_, s)| s.last_active())
                .map(|(i, _)| i)
            else {
                break;
            };
            live.swap_remove(oldest)
                .stop_with(messages::mcp::session_limit(spec.max_sessions));
        }
    }
}

/// Remove the session `state` of the server `name` from the registry of
/// the host `shared`, if both still exist.
fn forget(shared: &Weak<Shared>, name: &str, state: Option<Rc<SessionState>>) {
    let (Some(shared), Some(state)) = (shared.upgrade(), state) else {
        return;
    };
    let mut sessions = shared.sessions.borrow_mut();
    if let Some(live) = sessions.get_mut(name) {
        live.retain(|s| !Rc::ptr_eq(s, &state));
        if live.is_empty() {
            sessions.remove(name);
        }
    }
    if sessions.is_empty() {
        shared.idle.notify_waiters();
    }
}

#[cfg(test)]
mod tests;
