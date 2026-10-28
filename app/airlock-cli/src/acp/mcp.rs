//! Editor MCP servers of ACP sessions.
//!
//! Bridges the MCP servers that the editor gives in its session requests
//! into the sandbox. The host runs the local servers and relays the remote
//! servers. The agent gets one HTTP address in the sandbox for each server.
//! Commands, environment values, URLs and headers stay on the host.
//! Supported servers are local (stdio) servers and remote servers (HTTP
//! and SSE). The other entries are removed, or kept as they are when the
//! user gives the agent all editor features.
//!
//! The servers of one session make a set with a random key. The key is
//! part of the sandbox addresses, thus the sessions of all proxies stay
//! apart. A set ends when the editor closes the session, loads it again,
//! or when the session request fails. All sets end with the proxy.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use airlock_common::cli_capnp::cli_service;
use airlock_common::supervisor_capnp::{mcp_grant, mcp_host};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::filter;
use crate::mcp::{HostStopper, McpHostImpl, RemoteKind, RemoteSpec, ServerSpec, Transport};
use crate::messages;
use crate::util::verified_path::VerifiedPath;

/// Session requests that have MCP servers.
pub(super) const SESSION_METHODS: &[&str] = &[
    "session/new",
    "session/load",
    "session/resume",
    "session/fork",
];

/// Host name of the sandbox addresses of the editor MCP servers.
const ROUTE_HOST: &str = "acp.mcp.airlock";

/// Maximum length of a server name in a sandbox address, in bytes.
const MAX_NAME: usize = 64;

/// Maximum number of live sessions of one editor MCP server.
const MAX_SESSIONS: usize = 16;

/// Time that the sandbox has to remove the addresses of a set.
const RELEASE_TIMEOUT: Duration = Duration::from_secs(5);

/// Registration of the servers of one session request that runs.
pub(super) type Registration = Pin<Box<dyn Future<Output = Registered>>>;

/// Session request with its MCP servers in the sandbox.
pub(super) struct Registered {
    /// The request for the agent, with the sandbox addresses.
    pub(super) msg: Value,
    /// Key of the session.
    key: String,
    /// The servers that the sandbox serves.
    set: ServerSet,
}

/// Session request that makes a set of servers.
pub(super) struct SessionRequest<'r> {
    /// Method of the request.
    pub(super) method: &'r str,
    /// Id of the request.
    pub(super) id: &'r Value,
    /// Session dir on the host, if the request has one.
    pub(super) host_cwd: Option<&'r str>,
    /// Remove the entries that the proxy cannot bridge.
    pub(super) restrict: bool,
    /// `initialize` result of the agent.
    pub(super) agent_init: &'r Value,
    /// CLI service of the sandbox that adds the sandbox addresses, or
    /// `None` if the agent has none.
    pub(super) registrar: Option<cli_service::Client>,
    /// Time limit of the release of an old set and the registration of
    /// all servers.
    pub(super) timeout: Duration,
    /// Sandbox of the agent, for the log.
    pub(super) sandbox: &'r str,
}

/// MCP servers in the sandbox for one ACP session.
struct ServerSet {
    /// Stops the server sessions on the host.
    stopper: Option<HostStopper>,
    /// Server names and their sandbox addresses.
    routes: Vec<(String, mcp_grant::Client)>,
}

impl ServerSet {
    /// Close the host of the set now: its server sessions stop and it
    /// opens no new ones.
    /// Returns:
    ///   Future that removes the sandbox addresses and waits for the end of
    ///   the server sessions, with a time limit. The addresses go away
    ///   only when the future runs, or when it drops.
    fn release(self) -> impl Future<Output = ()> + 'static {
        if let Some(stopper) = &self.stopper {
            stopper.close(messages::acp::MCP_SESSION_ENDED);
        }
        async move {
            let calls = self
                .routes
                .iter()
                .map(|(_, grant)| grant.release_request().send().promise);
            let stopped = async {
                if let Some(stopper) = &self.stopper {
                    stopper.stopped().await;
                }
            };
            let done = futures::future::join(futures::future::join_all(calls), stopped);
            if tokio::time::timeout(RELEASE_TIMEOUT, done).await.is_err() {
                tracing::debug!("MCP release did not complete");
            }
        }
    }
}

/// What a pending session request does at its response.
enum Pending {
    /// A request that made the set `key`. An OK response maps the session
    /// to the key. An error releases the key.
    Open {
        /// Key of the set.
        key: String,
        /// Session of a load or resume. `None`: the response gives it.
        session: Option<String>,
    },
    /// A close of a session. An OK response releases its set.
    Close {
        /// The ACP session id.
        session: String,
    },
}

/// Server sets of the ACP sessions of one proxy.
#[derive(Default)]
pub(super) struct Registry {
    /// Key of each known ACP session.
    sessions: HashMap<String, String>,
    /// Server set of each key.
    sets: HashMap<String, ServerSet>,
    /// Session requests by the JSON text of their id.
    pending: HashMap<String, Pending>,
    /// Releases that run in the background. The proxy end waits for them.
    background: Vec<JoinHandle<()>>,
}

impl Registry {
    /// Prepare a session request: give it a key and register its servers.
    /// Args:
    ///  - `request`: The session request
    ///  - `msg`: The request message for the agent
    ///
    /// Returns:
    ///   Registration that gives the request with the sandbox addresses,
    ///   or the error code and message for the client.
    pub(super) fn prepare(
        &mut self,
        request: SessionRequest<'_>,
        msg: Value,
    ) -> Result<Registration, (i64, String)> {
        let reload = matches!(request.method, "session/load" | "session/resume");
        let session = reload
            .then(|| msg.pointer("/params/sessionId").and_then(Value::as_str))
            .flatten()
            .map(str::to_string);
        let known = session.as_ref().and_then(|s| self.sessions.get(s)).cloned();
        let key = match known {
            Some(key) => key,
            None => new_key().map_err(|e| (filter::INTERNAL_ERROR, format!("{e:#}")))?,
        };
        // A load or resume of a known session keeps its addresses. The old
        // set goes first: the new set uses the same addresses.
        let old = self.sets.remove(&key).map(ServerSet::release);
        self.track(
            request.id,
            Pending::Open {
                key: key.clone(),
                session,
            },
        );
        let (slots, specs) = plan(&request, &msg);
        let names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
        tracing::info!(
            sandbox = request.sandbox,
            method = request.method,
            key,
            names = ?names,
            "register MCP servers"
        );
        let registrar = request.registrar;
        // One time limit for the release of the old set and the new
        // registrations. Thus the client waits at most this time.
        let deadline = Instant::now() + request.timeout;
        let cwd = request.host_cwd.map(str::to_string);
        Ok(Box::pin(async move {
            if let Some(old) = old
                && tokio::time::timeout_at(deadline, old).await.is_err()
            {
                tracing::debug!(key, "old MCP servers did not end in time");
            }
            let set = match (registrar, cwd) {
                (Some(registrar), Some(cwd)) if !specs.is_empty() => {
                    register(&registrar, &key, specs, &cwd, deadline).await
                }
                _ => ServerSet {
                    stopper: None,
                    routes: Vec::new(),
                },
            };
            let msg = rewrite(msg, slots, &key, &set);
            let names: Vec<&str> = set.routes.iter().map(|(n, _)| n.as_str()).collect();
            tracing::info!(key, names = ?names, "MCP servers registered");
            Registered { msg, key, set }
        }))
    }

    /// Keep the set of a completed registration.
    /// Returns:
    ///   The request for the agent.
    pub(super) fn add(&mut self, registered: Registered) -> Value {
        let Registered { msg, key, set } = registered;
        // A key has one set. An old one can come from a second request
        // with the same key.
        if let Some(old) = self.sets.insert(key, set) {
            self.spawn_release(old);
        }
        msg
    }

    /// Remember the session request `id` until its response. A client
    /// should not use an id two times. If it does, the last request wins.
    fn track(&mut self, id: &Value, pending: Pending) {
        if self.pending.insert(id_text(id), pending).is_some() {
            tracing::warn!(id = %id, "two session requests with the same id");
        }
    }

    /// Release `set` in a task. The proxy end waits for the task.
    fn spawn_release(&mut self, set: ServerSet) {
        self.background.retain(|task| !task.is_finished());
        self.background
            .push(tokio::task::spawn_local(set.release()));
    }

    /// Remember a `session/close` request, to release the set of the
    /// session at the OK response.
    pub(super) fn track_close(&mut self, id: &Value, params: &Value) {
        let Some(session) = params.get("sessionId").and_then(Value::as_str) else {
            return;
        };
        if self.sessions.contains_key(session) {
            let session = session.to_string();
            self.track(id, Pending::Close { session });
        }
    }

    /// Handle an agent response: map a new session to its key, or release
    /// the set of a failed or closed session.
    pub(super) fn on_response(&mut self, id: &Value, msg: &Value) {
        let Some(pending) = self.pending.remove(&id_text(id)) else {
            return;
        };
        let ok = msg.get("error").is_none();
        match pending {
            Pending::Open { key, session } => {
                let session = session.or_else(|| {
                    msg.pointer("/result/sessionId")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                });
                match (ok, session) {
                    (true, Some(session)) => {
                        if let Some(old) = self.sessions.insert(session, key.clone())
                            && old != key
                        {
                            self.release(&old);
                        }
                    }
                    // The agent did not give the session id. The set then
                    // stays until the proxy ends.
                    (true, None) => {
                        tracing::debug!(key, "no session id in the response");
                    }
                    (false, session) => {
                        tracing::debug!(key, "release MCP servers of failed request");
                        // A failed load or resume ends the known session.
                        if let Some(session) = session
                            && self.sessions.get(&session) == Some(&key)
                        {
                            self.sessions.remove(&session);
                        }
                        self.release(&key);
                    }
                }
            }
            Pending::Close { session } if ok => {
                if let Some(key) = self.sessions.remove(&session) {
                    tracing::info!(key, "release MCP servers of closed session");
                    self.release(&key);
                }
            }
            Pending::Close { .. } => {}
        }
    }

    /// Release the set `key` in the background, if it exists.
    fn release(&mut self, key: &str) {
        if let Some(set) = self.sets.remove(key) {
            self.spawn_release(set);
        }
    }

    /// Release all sets and forget all sessions.
    /// Returns:
    ///   Future that completes when all releases, also the ones in the
    ///   background, are done or reached their time limit.
    pub(super) fn release_all(&mut self) -> impl Future<Output = ()> + 'static {
        self.sessions.clear();
        self.pending.clear();
        if !self.sets.is_empty() {
            tracing::info!(sets = self.sets.len(), "release all MCP servers");
        }
        let releases: Vec<_> = self.sets.drain().map(|(_, s)| s.release()).collect();
        let background = std::mem::take(&mut self.background);
        async move {
            futures::future::join(
                futures::future::join_all(releases),
                futures::future::join_all(background),
            )
            .await;
        }
    }
}

/// Remove all MCP server entries of the request params.
/// Returns:
///   Number of removed entries.
pub(super) fn remove_all(params: &mut Value) -> usize {
    let Some(servers) = params.get_mut("mcpServers").and_then(Value::as_array_mut) else {
        return 0;
    };
    std::mem::take(servers).len()
}

/// Entry of the rewritten `mcpServers` list.
enum Slot {
    /// Entry that goes to the agent as it is.
    Keep(Value),
    /// Bridged server with its original name, and its name in the address.
    Bridge { name: String, route: String },
}

/// Parse the MCP servers of a session request.
/// Returns:
///   The entries of the new list, and the servers to bridge.
fn plan(request: &SessionRequest<'_>, msg: &Value) -> (Vec<Slot>, Vec<ServerSpec>) {
    let entries = msg
        .pointer("/params/mcpServers")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    let sandbox = request.sandbox;
    let mut slots = Vec::new();
    let mut specs: Vec<ServerSpec> = Vec::new();
    for entry in entries {
        match parse(entry) {
            Parsed::Bridge { name, transport } => {
                let route = unique_name(&name, &specs);
                slots.push(Slot::Bridge {
                    name,
                    route: route.clone(),
                });
                specs.push(ServerSpec {
                    name: route,
                    transport,
                    timeout: None,
                    max_sessions: MAX_SESSIONS,
                });
            }
            Parsed::Other(reason) if request.restrict => {
                let name = log_name(entry);
                tracing::warn!(sandbox, name, "drop editor MCP server: {reason}");
            }
            Parsed::Other(_) => slots.push(Slot::Keep(entry.clone())),
            Parsed::Invalid(reason) => {
                let name = log_name(entry);
                tracing::warn!(sandbox, name, "drop editor MCP server: {reason}");
            }
        }
    }
    if specs.is_empty() {
        return (slots, specs);
    }
    let has_http = request
        .agent_init
        .pointer("/agentCapabilities/mcpCapabilities/http")
        .and_then(Value::as_bool)
        == Some(true);
    let problem = if !has_http {
        Some("the agent does not support HTTP MCP servers")
    } else if request.registrar.is_none() {
        Some("no connection to the sandbox")
    } else if request.host_cwd.is_none() {
        Some("the request has no session dir")
    } else {
        None
    };
    if let Some(problem) = problem {
        tracing::warn!(
            sandbox,
            count = specs.len(),
            "drop all bridged MCP servers: {problem}"
        );
        slots.retain(|s| matches!(s, Slot::Keep(_)));
        specs.clear();
    }
    for spec in &specs {
        match &spec.transport {
            Transport::Stdio { command, .. } => {
                tracing::debug!(
                    sandbox,
                    name = spec.name,
                    command,
                    "bridge stdio MCP server"
                );
            }
            Transport::Remote(remote) => {
                let origin = remote.url.origin().ascii_serialization();
                tracing::debug!(
                    sandbox,
                    name = spec.name,
                    origin,
                    "bridge remote MCP server"
                );
            }
        }
    }
    (slots, specs)
}

/// One entry of `mcpServers` after the parse.
enum Parsed {
    /// Server that the proxy bridges.
    Bridge { name: String, transport: Transport },
    /// Entry of a kind that the proxy does not bridge, with the reason.
    Other(String),
    /// Entry that is not valid, with the reason.
    Invalid(String),
}

/// Parse one entry of `mcpServers`.
fn parse(entry: &Value) -> Parsed {
    let kind = match entry.get("type") {
        None => Some("stdio"),
        Some(Value::String(kind)) => Some(kind.as_str()),
        Some(_) => None,
    };
    let Some(name) = entry.get("name").and_then(Value::as_str) else {
        return Parsed::Invalid("no name".into());
    };
    let transport = match kind {
        Some("stdio") => stdio(entry),
        Some("http") => remote(entry, RemoteKind::Http),
        Some("sse") => remote(entry, RemoteKind::Sse),
        Some(kind) => return Parsed::Other(format!("type '{kind}' is not bridged")),
        None => return Parsed::Invalid("the type is not a string".into()),
    };
    match transport {
        Ok(transport) => Parsed::Bridge {
            name: name.to_string(),
            transport,
        },
        Err(reason) => Parsed::Invalid(reason),
    }
}

/// Parse a stdio server entry.
fn stdio(entry: &Value) -> Result<Transport, String> {
    let command = entry
        .get("command")
        .and_then(Value::as_str)
        .filter(|c| !c.is_empty())
        .ok_or("no command")?;
    let args = match entry.get("args") {
        None => Vec::new(),
        Some(args) => args
            .as_array()
            .and_then(|a| {
                a.iter()
                    .map(|v| v.as_str().map(str::to_string))
                    .collect::<Option<Vec<_>>>()
            })
            .ok_or("args are not a list of strings")?,
    };
    Ok(Transport::Stdio {
        command: command.to_string(),
        args,
        env: pairs(entry, "env")?,
    })
}

/// Parse a remote server entry.
fn remote(entry: &Value, kind: RemoteKind) -> Result<Transport, String> {
    let url = entry.get("url").and_then(Value::as_str).ok_or("no url")?;
    let headers = pairs(entry, "headers")?;
    Ok(Transport::Remote(RemoteSpec::new(kind, url, &headers)?))
}

/// Parse a list of `{name, value}` objects in the field `field` of an
/// entry. A missing field is an empty list.
fn pairs(entry: &Value, field: &str) -> Result<Vec<(String, String)>, String> {
    let Some(list) = entry.get(field) else {
        return Ok(Vec::new());
    };
    list.as_array()
        .and_then(|list| {
            list.iter()
                .map(|item| {
                    let name = item.get("name")?.as_str()?;
                    let value = item.get("value")?.as_str()?;
                    Some((name.to_string(), value.to_string()))
                })
                .collect::<Option<Vec<_>>>()
        })
        .ok_or_else(|| format!("{field} is not a list of name and value"))
}

/// Make the name of a server in its sandbox address: characters other than
/// `[A-Za-z0-9_-]` become `_`, the name is cut to [`MAX_NAME`] bytes, and
/// an empty name becomes `mcp`. A name that `taken` has gets a suffix
/// `-2`, `-3` and so on.
fn unique_name(name: &str, taken: &[ServerSpec]) -> String {
    let mut base: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    // The name has only ASCII characters now. Thus each cut is on a char
    // boundary.
    base.truncate(MAX_NAME);
    if base.is_empty() {
        base = "mcp".into();
    }
    let is_free = |n: &str| !taken.iter().any(|s| s.name == n);
    if is_free(&base) {
        return base;
    }
    // `taken` has fewer names than this range. Thus one name is free.
    (2..=taken.len() + 2)
        .map(|i| {
            let suffix = format!("-{i}");
            let mut name = base.clone();
            name.truncate(MAX_NAME - suffix.len());
            name + &suffix
        })
        .find(|n| is_free(n))
        .expect("free name")
}

/// Name of an entry for the log, without control characters.
fn log_name(entry: &Value) -> String {
    let name = entry.get("name").and_then(Value::as_str).unwrap_or("-");
    name.chars().filter(|c| !c.is_control()).collect()
}

/// Register the servers `specs` in the sandbox under the key `key`, all at
/// the same time until `deadline`. Servers that fail or do not complete in
/// time are dropped with a warning.
/// Returns:
///   The set of the registered servers.
async fn register(
    registrar: &cli_service::Client,
    key: &str,
    specs: Vec<ServerSpec>,
    host_cwd: &str,
    deadline: Instant,
) -> ServerSet {
    let empty = || ServerSet {
        stopper: None,
        routes: Vec::new(),
    };
    // The client picks the session dir, the same as for the agent start.
    // Thus symlinks in it are followed.
    let cwd = match VerifiedPath::new_follow(host_cwd, None) {
        Ok(cwd) => cwd,
        Err(e) => {
            tracing::warn!(key, "drop all bridged MCP servers: session dir: {e}");
            return empty();
        }
    };
    let names: Vec<String> = specs.iter().map(|s| s.name.clone()).collect();
    let host = McpHostImpl::new(specs, cwd);
    let stopper = host.stopper();
    let host: mcp_host::Client = capnp_rpc::new_client(host);
    let calls = names.iter().map(|name| {
        let mut req = registrar.add_mcp_server_request();
        req.get().set_session(key);
        req.get().set_name(name);
        req.get().set_host(host.clone());
        async move {
            match tokio::time::timeout_at(deadline, req.send().promise).await {
                Ok(Ok(response)) => response
                    .get()
                    .and_then(cli_service::add_mcp_server_results::Reader::get_grant)
                    .map_err(|e| e.to_string()),
                Ok(Err(e)) => Err(e.to_string()),
                Err(_) => Err("no reply in time".to_string()),
            }
        }
    });
    let results = futures::future::join_all(calls).await;
    let mut routes = Vec::new();
    for (name, result) in names.into_iter().zip(results) {
        match result {
            Ok(grant) => routes.push((name, grant)),
            Err(e) => tracing::warn!(key, name, "drop editor MCP server: {e}"),
        }
    }
    ServerSet {
        stopper: Some(stopper),
        routes,
    }
}

/// Make the `mcpServers` list for the agent: the kept entries and an HTTP
/// entry for each registered server.
fn rewrite(mut msg: Value, slots: Vec<Slot>, key: &str, set: &ServerSet) -> Value {
    let Some(servers) = msg
        .pointer_mut("/params/mcpServers")
        .and_then(Value::as_array_mut)
    else {
        return msg;
    };
    *servers = slots
        .into_iter()
        .filter_map(|slot| match slot {
            Slot::Keep(entry) => Some(entry),
            Slot::Bridge { name, route } => {
                set.routes.iter().any(|(n, _)| *n == route).then(|| {
                    json!({
                        "type": "http",
                        "name": name,
                        "url": format!("http://{ROUTE_HOST}/{key}/{route}"),
                        "headers": [],
                    })
                })
            }
        })
        .collect();
    msg
}

/// Make a new random session key: 16 hex characters.
fn new_key() -> anyhow::Result<String> {
    Ok(hex::encode(crate::services::store::random_bytes::<8>()?))
}

/// The JSON text of a request id. A response has the same id value.
fn id_text(id: &Value) -> String {
    id.to_string()
}
