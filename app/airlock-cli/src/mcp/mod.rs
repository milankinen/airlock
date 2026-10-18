//! Host stdio MCP servers.
//!
//! Gives the guest access to the MCP servers that the project config
//! enables. The guest serves each server as an HTTP endpoint. The host
//! runs the server program and relays the JSON-RPC messages between the
//! guest and the program, one message for each line.
//!
//! Each guest MCP session gets its own server process. The process stops
//! when the guest releases the session, and the guest learns when the
//! process stops. The host also stops sessions that are idle for too long,
//! and the session with the oldest activity when a server has too many
//! sessions. The guest can start only the servers that the config
//! enables.
//!
//! The environment templates of the servers resolve on the host at the
//! sandbox start. Their values never go to the guest.

mod session;

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::supervisor_capnp::mcp_host;

use crate::config::config_values::McpServer;
use crate::mcp::session::SessionState;
use crate::messages;
use crate::project::Project;
use crate::util::VerifiedPath;
use crate::vault::Vault;

/// One enabled MCP server, ready to start.
#[derive(Debug, Clone)]
struct ServerSpec {
    /// Name from `[mcp.<name>]`.
    name: String,
    /// Program to run.
    command: String,
    /// Arguments of the program.
    args: Vec<String>,
    /// Resolved environment values, on top of the host environment.
    env: Vec<(String, String)>,
    /// Idle time after which the host stops a session. `None`: no limit.
    timeout: Option<Duration>,
    /// Maximum number of live sessions, 1 or more.
    max_sessions: usize,
}

/// Live sessions by server name.
type Sessions = BTreeMap<String, Vec<Rc<SessionState>>>;

/// Cap'n Proto `McpHost` server that starts the host MCP servers for the
/// guest.
///
/// Each call checks the server name again here. It does not trust
/// anything in the sandbox.
pub struct McpHostImpl {
    /// Enabled servers by name.
    servers: BTreeMap<String, ServerSpec>,
    /// Working directory of the server processes.
    cwd: VerifiedPath,
    /// Live sessions of each server, to apply `max_sessions`. A session
    /// removes itself when it ends.
    sessions: Rc<RefCell<Sessions>>,
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
            let spec = ServerSpec {
                name: name.clone(),
                command: s.command.clone(),
                args: s.args.clone(),
                env,
                timeout: (s.timeout > 0).then(|| Duration::from_secs(s.timeout.into())),
                max_sessions: usize::try_from(s.max_sessions).unwrap_or(usize::MAX).max(1),
            };
            Ok((name.clone(), spec))
        })
        .collect::<anyhow::Result<BTreeMap<_, _>>>()?;
    if servers.is_empty() {
        return Ok(None);
    }
    Ok(Some(McpHostImpl {
        servers,
        cwd: cwd.clone(),
        sessions: Rc::default(),
    }))
}

impl McpHostImpl {
    /// Names of the enabled servers, in name order.
    pub(crate) fn names(&self) -> Vec<String> {
        self.servers.keys().cloned().collect()
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
        let sink = params.get_sink()?;
        // The session removes itself from the registry when it ends. The
        // registry does not keep it.
        let registry = Rc::downgrade(&self.sessions);
        let name = spec.name.clone();
        let ended: Rc<RefCell<Option<Rc<SessionState>>>> = Rc::default();
        let on_end = {
            let ended = ended.clone();
            move || {
                if let (Some(registry), Some(state)) = (registry.upgrade(), ended.take()) {
                    forget(&mut registry.borrow_mut(), &name, &state);
                }
            }
        };
        let session = session::start(spec, &self.cwd, sink, on_end)?;
        let state = session.state();
        *ended.borrow_mut() = Some(state.clone());
        // Make room only after the new session started. Thus a failed
        // start does not stop a working session.
        self.make_room(spec, state);
        results.get().set_session(capnp_rpc::new_client(session));
        Ok(())
    }
}

impl McpHostImpl {
    /// Add the new session `added` of `spec`. If the server then has more
    /// than `max_sessions` running sessions, stop the others with the oldest
    /// activity. A busy session can also stop: else a guest could keep the
    /// limit full.
    fn make_room(&self, spec: &ServerSpec, added: Rc<SessionState>) {
        let mut sessions = self.sessions.borrow_mut();
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

/// Remove the session `state` of the server `name` from `sessions`.
fn forget(sessions: &mut Sessions, name: &str, state: &Rc<SessionState>) {
    if let Some(live) = sessions.get_mut(name) {
        live.retain(|s| !Rc::ptr_eq(s, state));
        if live.is_empty() {
            sessions.remove(name);
        }
    }
}

#[cfg(test)]
mod tests;
