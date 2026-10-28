//! Message relay between the client and the agent.
//!
//! Reads the client messages line by line and answers the handshake before
//! the agent runs. Starts the agent at the first session request and does
//! the agent handshake with the client params. Then relays the messages in
//! both directions and applies the message rules. Stops the agent when the
//! client closes, and gives the exit code of the agent.
//!
//! The editor MCP servers of a session request go to the sandbox before
//! the agent gets the request. In that time, the proxy does not read more
//! client lines, but still relays the agent output and the signals. The
//! client lines keep their order.
//!
//! The memory use has a limit. The proxy stops an agent that writes a too
//! long line or does not read its input. A slow client slows the agent.
//! The agent stderr never stops the proxy. When the host stderr is slow,
//! the proxy drops the agent stderr.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use futures::StreamExt;
use serde_json::Value;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TrySendError;
use tokio::task::JoinHandle;
use tokio::time::Instant;

use super::filter::{self, AgentVerdict, Kind};
use super::{AgentProcess, AgentStarter, Control, Location, mcp};
use crate::runtime::SignalStream;
use crate::{messages, rpc};

/// Settings of the proxy core.
pub struct ProxyConfig {
    /// Agent command and its arguments.
    pub argv: Vec<String>,
    /// Agent command as the user gave it, for messages.
    pub name: String,
    /// Agent name that the client shows.
    pub title: String,
    /// Version of airlock.
    pub version: String,
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
    /// Time that the agent has to exit after the client closes, and again
    /// after the kill.
    pub shutdown_grace: Duration,
    /// Source of the host signals, or `None` to not watch them. The proxy
    /// watches them from the first agent start. Before the handshake
    /// completes, a signal stops the agent and the proxy. After it, the
    /// agent gets the signals.
    pub signals: Option<fn() -> anyhow::Result<SignalStream>>,
    /// Maximum length of one agent output line.
    pub max_line_bytes: usize,
}

/// Maximum number of parts that wait for the client output or the host
/// stderr. A full client queue stops the proxy until the client reads. A
/// full stderr queue drops the agent stderr.
const WRITER_QUEUE: usize = 4;

/// Maximum number of client lines that wait for the proxy. A full queue
/// stops the read of the client input.
const READER_QUEUE: usize = 16;

/// Agent start that runs.
type Launch<'a> = Pin<Box<dyn Future<Output = anyhow::Result<(Location, AgentProcess)>> + 'a>>;

/// Run the proxy between a client and the agent.
/// Args:
///  - `starter`: Starts the agent at the first session request
///  - `config`: Proxy settings
///  - `input`: Client messages, one per line
///  - `output`: Destination of the messages to the client
///  - `errors`: Destination of the agent stderr
///
/// Returns:
///   Exit code of the agent, or 0 if the client closed before an agent
///   ran.
pub async fn serve<S, R, W, E>(
    starter: &S,
    config: &ProxyConfig,
    input: R,
    output: W,
    errors: E,
) -> anyhow::Result<i32>
where
    S: AgentStarter,
    R: AsyncBufRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
    E: AsyncWrite + Unpin + 'static,
{
    let (out, out_task) = spawn_writer(output);
    let (err, mut err_task) = spawn_writer(errors);
    let mut proxy = Proxy {
        starter,
        config,
        out,
        err,
        client: spawn_reader(input),
        init_params: None,
        phase: Phase::Idle,
        agent: None,
        launch: None,
        deadline: None,
        signals: None,
        err_dropped: 0,
        mcp: mcp::Registry::default(),
        registration: None,
        held: None,
    };
    let code = proxy.run().await;
    proxy.end_mcp().await;
    if proxy.err_dropped > 0 {
        tracing::info!(bytes = proxy.err_dropped, "dropped agent stderr");
    }
    // Write all messages before the exit.
    drop(proxy);
    let _ = out_task.await;
    // A stderr that nobody reads must not stop the exit.
    if tokio::time::timeout(config.shutdown_grace, &mut err_task)
        .await
        .is_err()
    {
        tracing::warn!("stderr write did not complete");
        err_task.abort();
    }
    Ok(code)
}

/// Sender or receiver of a message, for the log.
#[derive(Clone, Copy)]
enum Peer {
    Client,
    Agent,
    Airlock,
}

impl Peer {
    /// Name of the peer in the log.
    fn name(self) -> &'static str {
        match self {
            Peer::Client => "client",
            Peer::Agent => "agent",
            Peer::Airlock => "airlock",
        }
    }
}

/// Log a message at the trace level, as `<from> -> <to>: <message>`.
/// The log does not show the env and header values of the editor MCP
/// servers in client messages.
fn trace_message(sandbox: &str, from: Peer, to: Peer, line: &[u8]) {
    if !tracing::enabled!(tracing::Level::TRACE) {
        return;
    }
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    let hidden = match from {
        Peer::Client => hide_mcp_secrets(line),
        Peer::Agent | Peer::Airlock => None,
    };
    tracing::trace!(
        sandbox,
        "{} -> {}: {}",
        from.name(),
        to.name(),
        String::from_utf8_lossy(hidden.as_deref().unwrap_or(line))
    );
}

/// Text that replaces a secret value in the log.
const HIDDEN: &str = "***";

/// Replace the env and header values of each editor MCP server in the
/// client message `line` with [`HIDDEN`].
/// Returns:
///   The changed line, or `None` if the line has no MCP server list.
fn hide_mcp_secrets(line: &[u8]) -> Option<Vec<u8>> {
    let mut msg: Value = serde_json::from_slice(line).ok()?;
    let servers = msg.pointer_mut("/params/mcpServers")?.as_array_mut()?;
    for server in servers {
        for field in ["env", "headers"] {
            match server.get_mut(field) {
                Some(Value::Array(items)) => {
                    for value in items.iter_mut().filter_map(|i| i.get_mut("value")) {
                        *value = HIDDEN.into();
                    }
                }
                // Not valid ACP, but the values can still be secrets.
                Some(Value::Object(map)) => {
                    for value in map.values_mut() {
                        *value = HIDDEN.into();
                    }
                }
                _ => {}
            }
        }
    }
    Some(msg.to_string().into_bytes())
}

/// Phase of the proxy.
enum Phase {
    /// No agent runs.
    Idle,
    /// The agent starts, or it runs and the handshake is not complete.
    Starting {
        /// Session request that started the agent.
        request: Vec<u8>,
        /// Id of the session request.
        id: Value,
        /// Client lines that came during the start.
        queued: Vec<Vec<u8>>,
    },
    /// The agent runs after a complete handshake.
    Running {
        /// Sandbox of the agent.
        sandbox: String,
        /// `initialize` result of the agent.
        agent_init: Value,
    },
}

/// Running agent process and its partial output line.
struct Agent {
    process: AgentProcess,
    /// Stdout bytes after the last complete line. They have no newline.
    partial: Vec<u8>,
    /// Sandbox of the agent.
    sandbox: String,
    /// The proxy killed the agent for a limit. Its output and input are
    /// dropped until it exits.
    stopping: bool,
}

/// State of one proxy run.
struct Proxy<'a, S> {
    starter: &'a S,
    config: &'a ProxyConfig,
    /// Lines to the client.
    out: mpsc::Sender<Vec<u8>>,
    /// Agent stderr to the host stderr.
    err: mpsc::Sender<Vec<u8>>,
    /// Number of agent stderr bytes that the proxy dropped.
    err_dropped: u64,
    /// Lines from the client. Closes at the client end of file.
    client: mpsc::Receiver<Vec<u8>>,
    /// Client `initialize` params after the filter.
    init_params: Option<Value>,
    phase: Phase,
    agent: Option<Agent>,
    /// Agent start that runs.
    launch: Option<Launch<'a>>,
    /// End of the time for the agent start and handshake.
    deadline: Option<Instant>,
    /// Host signals from the first agent start.
    signals: Option<SignalStream>,
    /// Editor MCP servers of the sessions.
    mcp: mcp::Registry,
    /// Registration of the editor MCP servers of a session request that
    /// runs.
    registration: Option<mcp::Registration>,
    /// Client lines that the proxy read before a registration started,
    /// and that come after its session request. They go to the agent after
    /// the session request.
    held: Option<Vec<Vec<u8>>>,
}

impl<'a, S: AgentStarter> Proxy<'a, S> {
    /// Relay the messages until the agent exits or the client closes.
    /// Returns:
    ///   The exit code.
    async fn run(&mut self) -> i32 {
        loop {
            // Each branch borrows a different field. A branch that is not
            // active waits forever. The branches are not biased: a busy
            // side must not stop the other side. Each side has one channel,
            // so the messages of one side keep their order.
            tokio::select! {
                // A registration does not keep the new client lines. The
                // full reader queue then stops the read of the client.
                line = self.client.recv(), if self.registration.is_none() => match line {
                    Some(line) => self.on_client_line(line).await,
                    None => return self.on_client_end().await,
                },
                started = next_launch(&mut self.launch) => {
                    self.launch = None;
                    self.on_launched(started).await;
                }
                registered = next_registration(&mut self.registration) => {
                    self.registration = None;
                    self.on_registered(registered).await;
                }
                event = next_event(&mut self.agent) => {
                    if let Some(code) = self.on_agent_event(event).await {
                        return code;
                    }
                }
                () = wait_until(self.deadline) => {
                    let secs = self.config.handshake_timeout.as_secs();
                    self.fail_start(&messages::acp::handshake_timeout(secs)).await;
                }
                signum = next_signal(&mut self.signals) => {
                    if let Some(code) = self.on_signal(signum).await {
                        return code;
                    }
                }
            }
        }
    }

    /// Handle one line from the client.
    async fn on_client_line(&mut self, line: Vec<u8>) {
        if let Some(held) = &mut self.held {
            held.push(line);
            return;
        }
        match &mut self.phase {
            Phase::Idle => self.on_idle_line(line).await,
            // The agent gets these lines in order after the handshake.
            Phase::Starting { queued, .. } => queued.push(line),
            Phase::Running { .. } => self.on_running_line(line).await,
        }
    }

    /// Handle one client line when no agent runs. Only requests get a
    /// reply. Notifications and responses have no target yet.
    async fn on_idle_line(&mut self, line: Vec<u8>) {
        let Ok(msg) = serde_json::from_slice::<Value>(&line) else {
            trace_message("-", Peer::Client, Peer::Airlock, &line);
            tracing::debug!("drop client line that is not JSON");
            return;
        };
        let Some(Kind::Request { id, method }) = filter::kind(&msg) else {
            trace_message("-", Peer::Client, Peer::Airlock, &line);
            tracing::debug!("drop client message before the agent start");
            return;
        };
        let id = id.clone();
        if method == "initialize" {
            trace_message("-", Peer::Client, Peer::Airlock, &line);
            let client = msg
                .pointer("/params/clientInfo/name")
                .and_then(Value::as_str);
            tracing::info!(client, "initialize");
            let mut params = msg.get("params").cloned().unwrap_or(Value::Null);
            filter::prepare_initialize_params(&mut params, self.config.restrict);
            self.init_params = Some(params);
            self.reply(&id, Ok(self.initialize_result())).await;
            return;
        }
        let Some(cwd) = msg.pointer("/params/cwd").and_then(Value::as_str) else {
            trace_message("-", Peer::Client, Peer::Airlock, &line);
            tracing::debug!(method, "refuse client request before the agent start");
            let error = (
                filter::INVALID_REQUEST,
                messages::acp::NO_SESSION_YET.into(),
            );
            self.reply(&id, Err(error)).await;
            return;
        };
        if self.init_params.is_none() {
            trace_message("-", Peer::Client, Peer::Airlock, &line);
            let error = (filter::INVALID_REQUEST, messages::acp::NO_HANDSHAKE.into());
            self.reply(&id, Err(error)).await;
            return;
        }
        if !cwd.starts_with('/') {
            trace_message("-", Peer::Client, Peer::Airlock, &line);
            let error = (filter::INVALID_PARAMS, messages::acp::cwd_not_absolute(cwd));
            self.reply(&id, Err(error)).await;
            return;
        }
        tracing::info!(method, cwd, "start the agent");
        // The log shows the request when the proxy sends it after the
        // handshake, or when the proxy refuses it.
        self.watch_signals();
        // The start can take long. The proxy handles the client and the
        // signals in that time.
        let (starter, config): (&'a S, &'a ProxyConfig) = (self.starter, self.config);
        let cwd = cwd.to_string();
        self.launch = Some(Box::pin(
            async move { starter.start(&cwd, &config.argv).await },
        ));
        self.deadline = Some(Instant::now() + config.handshake_timeout);
        self.phase = Phase::Starting {
            request: line,
            id,
            queued: Vec::new(),
        };
    }

    /// Handle the end of the agent start: send the agent handshake, or
    /// fail the session request.
    async fn on_launched(&mut self, started: anyhow::Result<(Location, AgentProcess)>) {
        let (location, process) = match started {
            Ok(started) => started,
            Err(e) => {
                self.fail_start(&format!("{e:#}")).await;
                return;
            }
        };
        // The arguments can hold secrets. Only the trace level logs them.
        tracing::info!(
            sandbox = location.sandbox,
            guest_cwd = location.guest_cwd,
            cmd = self.config.argv.first(),
            "agent started"
        );
        tracing::trace!(argv = ?self.config.argv, "agent command");
        self.agent = Some(Agent {
            process,
            partial: Vec::new(),
            sandbox: location.sandbox,
            stopping: false,
        });
        let params = self.init_params.clone().unwrap_or(Value::Null);
        let init = filter::request(filter::INIT_ID, "initialize", &params);
        self.send_agent(Peer::Airlock, to_line(&init));
    }

    /// Watch the host signals, if the config asks for it and the proxy
    /// does not watch them yet.
    // The proxy watches the signals only from the first agent start. Before
    // that, a signal stops the proxy as usual.
    fn watch_signals(&mut self) {
        let Some(source) = self.config.signals else {
            return;
        };
        if self.signals.is_some() {
            return;
        }
        match source() {
            Ok(signals) => self.signals = Some(signals),
            Err(e) => tracing::warn!("no signal watch: {e}"),
        }
    }

    /// Handle one host signal. After the handshake, send it to the agent.
    /// Before it, stop the agent and the proxy.
    /// Returns:
    ///   The exit code when the proxy must stop.
    async fn on_signal(&mut self, signum: i32) -> Option<i32> {
        if matches!(self.phase, Phase::Running { .. }) {
            tracing::debug!(sandbox = self.sandbox(), signum, "forward signal");
            self.control(Control::Signal(signum));
            return None;
        }
        tracing::info!(
            sandbox = self.sandbox(),
            signum,
            "signal before the handshake"
        );
        self.launch = None;
        if self.agent.is_some() {
            self.control(Control::Kill);
            self.wait_exit(self.config.shutdown_grace).await;
        }
        Some(128 + signum)
    }

    /// Handle one client line when the agent runs: check it, map the
    /// session dir to the guest and send it to the agent.
    async fn on_running_line(&mut self, line: Vec<u8>) {
        let Phase::Running {
            sandbox,
            agent_init,
        } = &self.phase
        else {
            return;
        };
        // The client is trusted. Lines that are not one JSON object go to
        // the agent as they are.
        let Ok(mut msg) = serde_json::from_slice::<Value>(&line) else {
            self.send_agent(Peer::Client, line);
            return;
        };
        let Some(Kind::Request { id, method }) = filter::kind(&msg) else {
            self.send_agent(Peer::Client, line);
            return;
        };
        let (id, method) = (id.clone(), method.to_string());
        // The agent did its handshake with the first params. A second
        // handshake must not change them.
        if method == "initialize" {
            trace_message(sandbox, Peer::Client, Peer::Airlock, &line);
            tracing::info!(sandbox, "repeated initialize");
            self.reply(&id, Ok(self.initialize_result())).await;
            return;
        }
        let mut changed = false;
        let sandbox = sandbox.clone();
        let agent_init = agent_init.clone();
        let host_cwd = msg
            .pointer("/params/cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(cwd) = &host_cwd {
            match self.guest_cwd(cwd, &sandbox).await {
                Ok(guest_cwd) => {
                    tracing::info!(method, cwd, sandbox, guest_cwd, "session request");
                    msg["params"]["cwd"] = Value::String(guest_cwd);
                }
                Err(error) => {
                    trace_message(&sandbox, Peer::Client, Peer::Airlock, &line);
                    tracing::info!(method, cwd, sandbox, "refuse session request: {}", error.1);
                    self.reply(&id, Err(error)).await;
                    return;
                }
            }
            changed = true;
        } else {
            tracing::debug!(method, sandbox, "client request");
        }
        let mut no_params = Value::Null;
        let params = msg.get_mut("params").unwrap_or(&mut no_params);
        if let Err(error) = filter::check_client_request(&method, params, &agent_init) {
            trace_message(&sandbox, Peer::Client, Peer::Airlock, &line);
            tracing::debug!(method, sandbox, "refuse client request: {}", error.1);
            self.reply(&id, Err(error)).await;
            return;
        }
        let bridge =
            self.config.proxy_mcp_servers && mcp::SESSION_METHODS.contains(&method.as_str());
        if !bridge {
            let removed = mcp::remove_all(params);
            if removed > 0 {
                tracing::info!(
                    method,
                    sandbox,
                    removed,
                    "remove editor MCP servers: --proxy-mcp-servers is not set"
                );
                changed = true;
            }
            if method == "session/close" {
                self.mcp.track_close(&id, params);
            }
            self.send_agent(Peer::Client, if changed { to_line(&msg) } else { line });
            return;
        }
        let request = mcp::SessionRequest {
            method: &method,
            id: &id,
            host_cwd: host_cwd.as_deref(),
            restrict: self.config.restrict,
            agent_init: &agent_init,
            registrar: self.agent.as_ref().and_then(|a| a.process.mcp.clone()),
            timeout: self.config.mcp_register_timeout,
            sandbox: &sandbox,
        };
        match self.mcp.prepare(request, msg) {
            // The request and the later client lines wait for the end of
            // the registration. The agent output and the signals do not.
            // Lines that the proxy read already wait in the held list.
            Ok(registration) => {
                self.registration = Some(registration);
                self.held = Some(Vec::new());
            }
            // The line can have env and header values. Thus the log does
            // not show it.
            Err(error) => {
                tracing::warn!(method, sandbox, "refuse session request: {}", error.1);
                self.reply(&id, Err(error)).await;
            }
        }
    }

    /// Send the session request with its sandbox MCP servers to the agent.
    /// Then handle the client lines that came in the time of the
    /// registration.
    async fn on_registered(&mut self, registered: mcp::Registered) {
        let msg = self.mcp.add(registered);
        self.send_agent(Peer::Client, to_line(&msg));
        for line in self.held.take().unwrap_or_default() {
            self.on_client_line(line).await;
        }
    }

    /// Release all editor MCP servers at the proxy end. Waits for the
    /// sandbox and the remote servers in the shutdown grace. A signal
    /// stops the wait.
    async fn end_mcp(&mut self) {
        // A registration that did not complete drops its servers. Its
        // request and the held lines never go to the agent, the same as the
        // lines that wait for the agent handshake.
        self.registration = None;
        for line in self.held.take().unwrap_or_default() {
            trace_message("-", Peer::Client, Peer::Airlock, &line);
        }
        let releases = self.mcp.release_all();
        let done = async {
            releases.await;
            crate::mcp::deletes_done().await;
        };
        tokio::select! {
            result = tokio::time::timeout(self.config.shutdown_grace, done) => {
                if result.is_err() {
                    tracing::info!("MCP server release did not complete");
                }
            }
            signum = next_signal(&mut self.signals) => {
                tracing::info!(signum, "skip MCP server release");
            }
        }
    }

    /// Get the guest path of the session dir `cwd`.
    /// Returns:
    ///   The guest path, or the error code and message if `cwd` is not an
    ///   absolute path or is not in the sandbox `sandbox`.
    async fn guest_cwd(&self, cwd: &str, sandbox: &str) -> Result<String, (i64, String)> {
        if !cwd.starts_with('/') {
            return Err((filter::INVALID_PARAMS, messages::acp::cwd_not_absolute(cwd)));
        }
        let location = self
            .starter
            .locate(cwd)
            .await
            .map_err(|e| (filter::INTERNAL_ERROR, format!("{e:#}")))?;
        // One agent runs in one sandbox. It cannot see the files of an
        // other sandbox.
        if location.sandbox != sandbox {
            return Err((filter::INVALID_PARAMS, messages::acp::other_sandbox(cwd)));
        }
        Ok(location.guest_cwd)
    }

    /// Handle one agent event.
    /// Returns:
    ///   The exit code when the proxy must stop.
    async fn on_agent_event(&mut self, event: rpc::ProcessEvent) -> Option<i32> {
        match event {
            rpc::ProcessEvent::Stdout(data) => {
                let max = self.config.max_line_bytes;
                let agent = self.agent.as_mut()?;
                if agent.stopping {
                    return None;
                }
                // The partial line has no newline. Thus only the new bytes
                // need a search, and the time stays linear for long lines.
                let end = data
                    .iter()
                    .rposition(|&b| b == b'\n')
                    .map(|pos| agent.partial.len() + pos);
                agent.partial.extend_from_slice(&data);
                let Some(end) = end else {
                    return self.check_partial(max).await;
                };
                let complete: Vec<u8> = agent.partial.drain(..=end).collect();
                for line in complete[..end].split(|&b| b == b'\n') {
                    if line.len() > max {
                        self.stop_agent(&messages::acp::agent_line_too_long(max))
                            .await;
                        return None;
                    }
                    self.on_agent_line(line).await;
                    // The line can stop the agent. Then the next lines
                    // have no target.
                    if self.agent.as_ref().is_none_or(|a| a.stopping) {
                        return None;
                    }
                }
                self.check_partial(max).await
            }
            rpc::ProcessEvent::Stderr(data) => {
                self.relay_stderr(data);
                None
            }
            rpc::ProcessEvent::Exit(code) => {
                tracing::info!(sandbox = self.sandbox(), code, "agent exited");
                self.flush_partial().await;
                if matches!(self.phase, Phase::Starting { .. }) {
                    self.fail_start(messages::acp::AGENT_EXITED).await;
                    return None;
                }
                self.agent = None;
                Some(code)
            }
        }
    }

    /// Stop the agent if its partial output line is longer than `max`.
    /// Returns:
    ///   Always `None`, as the agent event handler.
    async fn check_partial(&mut self, max: usize) -> Option<i32> {
        // A line without its end must not fill the memory.
        if self.agent.as_ref().is_some_and(|a| a.partial.len() > max) {
            self.stop_agent(&messages::acp::agent_line_too_long(max))
                .await;
        }
        None
    }

    /// Handle the last agent output line when it has no newline. Call it
    /// at each agent exit.
    async fn flush_partial(&mut self) {
        let Some(agent) = &mut self.agent else {
            return;
        };
        if agent.stopping || agent.partial.is_empty() {
            return;
        }
        let line = std::mem::take(&mut agent.partial);
        self.on_agent_line(&line).await;
    }

    /// Send agent stderr to the host stderr. Drop it if the stderr queue is
    /// full.
    // The proxy must not wait for the host stderr. An editor that does not
    // read it would stop the relay and the signal handling.
    fn relay_stderr(&mut self, data: Vec<u8>) {
        let len = data.len() as u64;
        if let Err(TrySendError::Full(_)) = self.err.try_send(data) {
            if self.err_dropped == 0 {
                tracing::warn!(
                    sandbox = self.sandbox(),
                    "drop agent stderr: stderr is full"
                );
            }
            self.err_dropped += len;
        }
    }

    /// Handle one line of agent output, without its newline.
    async fn on_agent_line(&mut self, line: &[u8]) {
        let parsed = serde_json::from_slice::<Value>(line).ok();
        // Only the handshake request of the proxy has this id. After the
        // handshake, the id goes to the client as any other.
        if let (Phase::Starting { .. }, Some(msg)) = (&self.phase, &parsed)
            && let Some(Kind::Response { id }) = filter::kind(msg)
            && id.as_str() == Some(filter::INIT_ID)
        {
            trace_message(self.sandbox(), Peer::Agent, Peer::Airlock, line);
            let msg = msg.clone();
            self.on_handshake_reply(msg).await;
            return;
        }
        // The editor MCP servers follow the session state in both modes.
        if let Some(msg) = &parsed
            && let Some(Kind::Response { id }) = filter::kind(msg)
        {
            self.mcp.on_response(id, msg);
        }
        if !self.config.restrict {
            if let Some(mut msg) = parsed
                && filter::add_auth_hint(&mut msg, &self.config.name)
            {
                self.to_client(Peer::Agent, &msg).await;
            } else {
                self.send_client(Peer::Agent, with_newline(line.to_vec()))
                    .await;
            }
            return;
        }
        // Only one JSON object at a time goes to the client, and as the
        // proxy encodes it. Thus the client cannot read a message that the
        // proxy did not check, for example in a batch.
        let Some(mut msg) = parsed.filter(Value::is_object) else {
            trace_message(self.sandbox(), Peer::Agent, Peer::Airlock, line);
            tracing::debug!(
                sandbox = self.sandbox(),
                "drop agent line that is not one JSON object"
            );
            return;
        };
        let method = msg
            .get("method")
            .and_then(Value::as_str)
            .map(str::to_string);
        let method = method.as_deref();
        match filter::check_agent_message(&msg) {
            AgentVerdict::Forward => {
                // Session updates come for each part of the agent output.
                // Only the trace level logs them.
                if method.is_some_and(|m| m != "session/update") {
                    tracing::debug!(method, sandbox = self.sandbox(), "agent message");
                }
                filter::add_auth_hint(&mut msg, &self.config.name);
                self.to_client(Peer::Agent, &msg).await;
            }
            AgentVerdict::Refuse { code, message } => {
                trace_message(self.sandbox(), Peer::Agent, Peer::Airlock, line);
                tracing::debug!(method, sandbox = self.sandbox(), "refuse agent request");
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let reply = filter::error_response(&id, code, &message);
                self.send_agent(Peer::Airlock, to_line(&reply));
            }
            AgentVerdict::Drop => {
                trace_message(self.sandbox(), Peer::Agent, Peer::Airlock, line);
                tracing::debug!(method, sandbox = self.sandbox(), "drop agent message");
            }
        }
    }

    /// Complete the agent handshake with the agent reply. Then send the
    /// session request and the queued client lines to the agent.
    async fn on_handshake_reply(&mut self, reply: Value) {
        if let Some(err) = reply.get("error") {
            self.fail_start(&messages::acp::agent_handshake_failed(err))
                .await;
            return;
        }
        let result = reply.get("result").cloned().unwrap_or(Value::Null);
        let version = result.get("protocolVersion").and_then(Value::as_u64);
        if version != Some(filter::PROTOCOL_VERSION) {
            let version = result.get("protocolVersion").unwrap_or(&Value::Null);
            let message =
                messages::acp::unsupported_protocol_version(version, filter::PROTOCOL_VERSION);
            self.fail_start(&message).await;
            return;
        }
        let Some(agent) = &self.agent else { return };
        let running = Phase::Running {
            sandbox: agent.sandbox.clone(),
            agent_init: result,
        };
        let Phase::Starting {
            request, queued, ..
        } = std::mem::replace(&mut self.phase, running)
        else {
            return;
        };
        self.deadline = None;
        tracing::info!(sandbox = self.sandbox(), "agent handshake complete");
        self.on_running_line(request).await;
        for line in queued {
            self.on_client_line(line).await;
        }
    }

    /// Kill the agent because it went over a limit. Before the handshake
    /// completes, fail the start with `message`. After it, the agent exit
    /// stops the proxy.
    async fn stop_agent(&mut self, message: &str) {
        if matches!(self.phase, Phase::Starting { .. }) {
            self.fail_start(message).await;
            return;
        }
        tracing::warn!(sandbox = self.sandbox(), "stop the agent: {message}");
        self.control(Control::Kill);
        if let Some(agent) = &mut self.agent {
            agent.stopping = true;
            agent.partial = Vec::new();
        }
    }

    /// Stop the agent that did not complete its start or handshake. Reply
    /// to the session request with `message`, and handle the queued client
    /// lines as when no agent runs.
    async fn fail_start(&mut self, message: &str) {
        let sandbox = self.sandbox().to_string();
        tracing::warn!(sandbox, "agent start failed: {message}");
        self.launch = None;
        self.control(Control::Kill);
        self.agent = None;
        self.deadline = None;
        // Servers are registered only after the handshake. Thus no set
        // exists here now. Release anyway, so that no set can outlive the
        // agent of its sandbox.
        self.registration = None;
        self.held = None;
        tokio::task::spawn_local(self.mcp.release_all());
        let Phase::Starting {
            request,
            id,
            queued,
        } = std::mem::replace(&mut self.phase, Phase::Idle)
        else {
            return;
        };
        trace_message(&sandbox, Peer::Client, Peer::Airlock, &request);
        self.reply(&id, Err((filter::INTERNAL_ERROR, message.to_string())))
            .await;
        for line in queued {
            self.on_client_line(line).await;
        }
    }

    /// Stop after the client closed.
    /// Returns:
    ///   Exit code of the agent, 0 if no agent completed its handshake, or
    ///   1 if the agent did not exit after the kill.
    async fn on_client_end(&mut self) -> i32 {
        tracing::info!(sandbox = self.sandbox(), "client closed");
        // The agent never gets the lines that wait for the handshake.
        if let Phase::Starting {
            request, queued, ..
        } = &self.phase
        {
            for line in std::iter::once(request).chain(queued) {
                trace_message(self.sandbox(), Peer::Client, Peer::Airlock, line);
            }
        }
        let Some(agent) = &mut self.agent else {
            return 0;
        };
        let starting = matches!(self.phase, Phase::Starting { .. });
        // An agent without a session has nothing to save. Kill it now.
        if starting {
            self.control(Control::Kill);
        } else {
            // The agent exits at the end of its input.
            let (closed, _) = mpsc::channel(1);
            drop(std::mem::replace(&mut agent.process.input, closed));
        }
        let grace = self.config.shutdown_grace;
        let mut code = self.wait_exit(grace).await;
        if code.is_none() {
            tracing::info!(sandbox = self.sandbox(), "kill the agent");
            self.control(Control::Kill);
            code = self.wait_exit(grace).await;
        }
        if starting { 0 } else { code.unwrap_or(1) }
    }

    /// Wait for the agent exit and handle its output in that time.
    /// Returns:
    ///   The exit code, or `None` if the agent did not exit in `limit`.
    async fn wait_exit(&mut self, limit: Duration) -> Option<i32> {
        let deadline = Instant::now() + limit;
        loop {
            tokio::select! {
                event = next_event(&mut self.agent) => {
                    if let rpc::ProcessEvent::Exit(code) = event {
                        tracing::info!(sandbox = self.sandbox(), code, "agent exited");
                        self.flush_partial().await;
                        self.agent = None;
                        return Some(code);
                    }
                    if let Some(code) = self.on_agent_event(event).await {
                        return Some(code);
                    }
                }
                () = tokio::time::sleep_until(deadline) => return None,
                // The user wants the proxy to stop now.
                signum = next_signal(&mut self.signals) => {
                    tracing::info!(sandbox = self.sandbox(), signum, "kill the agent");
                    self.control(Control::Kill);
                }
            }
        }
    }

    /// Send a control request to the agent, if it runs.
    fn control(&self, request: Control) {
        if let Some(agent) = &self.agent {
            let _ = agent.process.control.send(request);
        }
    }

    /// Send a line to the agent, if it runs. Kills the agent if its input
    /// queue is full.
    /// Args:
    ///  - `from`: Sender of the line, for the log
    ///  - `line`: The line, with or without its newline
    fn send_agent(&mut self, from: Peer, line: Vec<u8>) {
        let Some(agent) = &mut self.agent else {
            return;
        };
        if agent.stopping {
            return;
        }
        let line = with_newline(line);
        trace_message(&agent.sandbox, from, Peer::Agent, &line);
        // The proxy does not wait for the agent. An agent that does not
        // read its input must not make the proxy keep lines without limit.
        // A full queue thus stops the agent. The exit then fails the start
        // or stops the proxy.
        if let Err(TrySendError::Full(_)) = agent.process.input.try_send(line) {
            tracing::warn!(
                sandbox = agent.sandbox,
                "stop the agent: {}",
                messages::acp::AGENT_INPUT_FULL
            );
            agent.stopping = true;
            agent.partial = Vec::new();
            let _ = agent.process.control.send(Control::Kill);
        }
    }

    /// Send a message from `from` to the client.
    async fn to_client(&self, from: Peer, msg: &Value) {
        self.send_client(from, to_line(msg)).await;
    }

    /// Send a line, with its newline, to the client. Waits while the
    /// client output queue is full.
    async fn send_client(&self, from: Peer, line: Vec<u8>) {
        trace_message(self.sandbox(), from, Peer::Client, &line);
        let _ = self.out.send(line).await;
    }

    /// The sandbox of the agent for the log, or `-` if no agent runs.
    fn sandbox(&self) -> &str {
        self.agent.as_ref().map_or("-", |a| a.sandbox.as_str())
    }

    /// Send a response to the client.
    /// Args:
    ///  - `id`: Id of the client request
    ///  - `result`: Result, or the error code and message
    async fn reply(&self, id: &Value, result: Result<Value, (i64, String)>) {
        let msg = match result {
            Ok(result) => filter::result_response(id, &result),
            Err((code, message)) => filter::error_response(id, code, &message),
        };
        self.to_client(Peer::Airlock, &msg).await;
    }

    /// The `initialize` result that the proxy gives to the client.
    fn initialize_result(&self) -> Value {
        filter::initialize_result(
            &self.config.title,
            &self.config.version,
            self.config.proxy_mcp_servers,
        )
    }
}

/// Wait for the end of the agent start. Waits forever if no start runs.
async fn next_launch(launch: &mut Option<Launch<'_>>) -> anyhow::Result<(Location, AgentProcess)> {
    match launch {
        Some(launch) => launch.as_mut().await,
        None => std::future::pending().await,
    }
}

/// Wait for the end of the MCP server registration. Waits forever if no
/// registration runs.
async fn next_registration(registration: &mut Option<mcp::Registration>) -> mcp::Registered {
    match registration {
        Some(registration) => registration.as_mut().await,
        None => std::future::pending().await,
    }
}

/// Wait for the next agent event. Waits forever if no agent runs.
async fn next_event(agent: &mut Option<Agent>) -> rpc::ProcessEvent {
    let Some(agent) = agent else {
        return std::future::pending().await;
    };
    // A relay that ends without the exit event lost the agent.
    agent
        .process
        .events
        .recv()
        .await
        .unwrap_or(rpc::ProcessEvent::Exit(1))
}

/// Wait until `deadline`. Waits forever if there is none.
async fn wait_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline).await,
        None => std::future::pending().await,
    }
}

/// Wait for the next host signal. Waits forever if there are no signals.
async fn next_signal(signals: &mut Option<SignalStream>) -> i32 {
    match signals.as_mut().map(|s| s.next()) {
        Some(next) => match next.await {
            Some(signum) => signum,
            None => std::future::pending().await,
        },
        None => std::future::pending().await,
    }
}

/// Read the client lines in a task. The lines keep their bytes, also when
/// they are not UTF-8. When [`READER_QUEUE`] lines wait, the read waits.
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
// One task writes all data. Thus the messages do not mix, and the async
// code never blocks on a full pipe. The queue has a small limit. A reader
// that is slow thus makes the proxy wait, and the data does not collect in
// memory.
fn spawn_writer<W: AsyncWrite + Unpin + 'static>(
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

/// Encode a JSON-RPC message as one line.
fn to_line(msg: &Value) -> Vec<u8> {
    with_newline(msg.to_string().into_bytes())
}

/// Add a newline to the end of `bytes` if it has none.
fn with_newline(mut bytes: Vec<u8>) -> Vec<u8> {
    if bytes.last() != Some(&b'\n') {
        bytes.push(b'\n');
    }
    bytes
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Test that the trace log of a client session request does not show
    /// the env and header values of the editor MCP servers. They can be
    /// API keys and tokens.
    ///   1. Make a session request with a stdio server with env and a
    ///      remote server with headers
    ///   2. Check that the values are hidden and the names stay
    #[test]
    fn session_request_trace_hides_env_and_header_values() {
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/p", "mcpServers": [
                {"name": "a", "command": "x", "args": [],
                 "env": [{"name": "API_KEY", "value": "secret-env"}]},
                {"type": "http", "name": "b", "url": "https://h/mcp",
                 "headers": [{"name": "Authorization", "value": "secret-header"}]},
            ]},
        });
        let hidden = hide_mcp_secrets(request.to_string().as_bytes()).unwrap();
        let text = String::from_utf8(hidden).unwrap();
        assert!(!text.contains("secret"), "{text}");
        let hidden: Value = serde_json::from_str(&text).unwrap();
        let servers = &hidden["params"]["mcpServers"];
        assert_eq!(
            servers[0]["env"][0],
            json!({"name": "API_KEY", "value": HIDDEN})
        );
        assert_eq!(servers[1]["headers"][0]["name"], "Authorization");
    }
}
