//! One session and its agent processes.
//!
//! Each session runs in its own task with its own agent process in the
//! sandbox of its session dir. The task launches the agent, does the agent
//! handshake with the client params, and then gives the agent the client
//! messages of the session, with guest session dirs. Client messages that
//! come before the handshake wait, in order. Applies the message rules to
//! both directions.
//!
//! A session that does not start with a session request is one-shot. It
//! runs only its first request, gets no other agent messages, and stops
//! its agent after the response.
//!
//! A session shows all notices of the sandbox as warnings, after the OK
//! response to its session request.
//!
//! When the agent exits by itself, the open requests get an error, and the
//! session gets a message. The client shows it as a session notice if it
//! can, else as agent text. A session with an ACP session then stays. Its
//! next client request launches a new agent and opens the ACP session there
//! again, with a resume or else a load. The client does not see this
//! request, its response, or the history that a load sends. The other
//! client messages wait until the session is open. If the reopen fails,
//! the waiting requests get an error, and the next request tries again. A
//! close of such a session needs no agent.
//!
//! When the proxy stops the session, the agent input closes. An agent that
//! does not exit in the grace time is killed. The memory use has a limit.
//! An agent that writes a too long line or does not read its input is
//! stopped. A slow client slows the agent.

use std::collections::{HashMap, VecDeque};
use std::rc::Rc;
use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::Instrument;

use super::super::capabilities::{self, AgentVerdict};
use super::super::proxy::{Claim, HandleId, HandleMessage};
use super::super::rpc::{self, Kind, RequestId, SessionId, key, session_id};
use super::super::{AgentProcessLauncher, Location, NoSandbox, mcp};
use super::Pool;
use super::output::Output;
use super::process::{Agent, StartFailure, check_handshake, reopen_method};
use crate::messages;
use crate::sandbox::NoticeReader;

/// Maximum number of request params characters in the start failure log.
const MAX_LOGGED_PARAMS: usize = 500;

/// Maximum number of client messages that wait for the reopen of the
/// session.
const MAX_REOPEN_WAITING: usize = 64;

/// Client messages of the session, in order.
type Inbox = mpsc::UnboundedReceiver<Value>;

/// MCP server registration of a session request that runs, if one runs.
/// The client messages wait until it completes.
type Registering = Option<mcp::Registration>;

/// Run the session `hid` until it ends.
/// Args:
///  - `pool`: Settings of the agents
///  - `cwd`: Host session dir of the first request
///  - `first`: The first request of the session
///  - `inbox`: The later client messages. Its end stops the session.
///
/// Returns:
///   The first exit code that is not 0 of the agents of the session, else
///   0.
pub(super) async fn run_session<S: AgentProcessLauncher + 'static>(
    pool: Rc<Pool<S>>,
    hid: HandleId,
    cwd: String,
    first: Value,
    mut inbox: Inbox,
) -> i32 {
    let mut session = Session::new(pool, hid, cwd, &first);
    let mut waiting = VecDeque::from([first]);
    loop {
        let reopening = session.reopen_key.is_some();
        match session.run_agent(&mut inbox, &mut waiting).await {
            AgentEnd::Stopped => break,
            AgentEnd::StartFailed(failure) if !reopening => {
                session.fail_start(&mut waiting, &failure).await;
                break;
            }
            AgentEnd::StartFailed(failure) => {
                session.fail_reopen(&mut waiting, &failure.message).await;
            }
            AgentEnd::ReopenFailed(detail) => session.fail_reopen(&mut waiting, &detail).await,
            AgentEnd::Lost => {}
        }
        if session.claimed.is_none() {
            break;
        }
        // The servers of the session were in the sandbox of the agent.
        session.release_mcp().await;
        let Some(trigger) = session.next_request(&mut inbox).await else {
            break;
        };
        waiting = VecDeque::from([trigger]);
        session.start_reopen();
    }
    session.finish(inbox).await
}

/// Why an agent of the session ended.
enum AgentEnd {
    /// The session ends: the proxy stopped it, or a one-shot request has
    /// its response.
    Stopped,
    /// The agent exited by itself after its handshake.
    Lost,
    /// The start or the handshake of the agent failed.
    StartFailed(StartFailure),
    /// The reopen of the ACP session failed, with this detail.
    ReopenFailed(String),
}

/// State of one session across its agents.
struct Session<S> {
    pool: Rc<Pool<S>>,
    /// Identity of the session for the proxy.
    hid: HandleId,
    /// Session dir of the first request on the host.
    cwd: String,
    /// Map key of the id of the first request.
    first_key: RequestId,
    /// Method of the first request.
    first_method: String,
    /// Session id in the params of the first request, if it has one.
    first_sid: Option<SessionId>,
    /// The first request loads or resumes a session.
    reload: bool,
    /// The first request is not a session request. The session ends after
    /// its response and owns no ACP sessions.
    one_shot: bool,
    /// Editor MCP servers of the session request, as the client gave them.
    /// They stay in memory only and are never logged.
    mcp_servers: Value,
    /// The ACP session that the session request started.
    claimed: Option<SessionId>,
    /// Map key of the reopen request while it has no response. The client
    /// does not see it, and the client messages wait for it.
    reopen_key: Option<RequestId>,
    /// The client requests in the agent without a response, by map key:
    /// the request id, and if the request is a `session/list`. A list
    /// response has guest session dirs.
    open: HashMap<RequestId, (Value, bool)>,
    /// Editor MCP servers of the session.
    mcp: mcp::Registry,
    /// First exit code that is not 0 of the agents.
    code: i32,
}

impl<S: AgentProcessLauncher + 'static> Session<S> {
    /// Make the session of the first request `first`.
    fn new(pool: Rc<Pool<S>>, hid: HandleId, cwd: String, first: &Value) -> Self {
        let method = first.get("method").and_then(Value::as_str);
        let one_shot = !matches!(
            method,
            Some("session/new" | "session/load" | "session/resume" | "session/fork")
        );
        let reload = matches!(method, Some("session/load" | "session/resume"));
        let mcp_servers = first
            .pointer("/params/mcpServers")
            .cloned()
            .unwrap_or_else(|| json!([]));
        Self {
            pool,
            hid,
            cwd,
            first_key: key(first.get("id").unwrap_or(&Value::Null)),
            first_method: method.unwrap_or_default().to_string(),
            first_sid: session_id(first).map(str::to_string),
            reload,
            one_shot,
            mcp_servers,
            claimed: None,
            reopen_key: None,
            open: HashMap::new(),
            mcp: mcp::Registry::default(),
            code: 0,
        }
    }

    /// Launch an agent and relay the messages of the session until the
    /// agent ends. If a reopen is due, the agent gets the reopen request
    /// after its handshake, before the client messages.
    /// Args:
    ///  - `inbox`: Client messages of the session
    ///  - `waiting`: Client messages for the agent, in order
    async fn run_agent(&mut self, inbox: &mut Inbox, waiting: &mut VecDeque<Value>) -> AgentEnd {
        let pool = self.pool.clone();
        let config = &pool.config;
        let cwd = self.cwd.clone();
        let timeout = messages::acp::handshake_timeout(config.handshake_timeout.as_secs());
        // The time limit includes the sandbox start.
        let deadline = Instant::now() + config.handshake_timeout;
        tracing::info!(cwd, "launch an agent");
        let launched = {
            let launch = pool.launcher.launch(&cwd, &config.argv);
            tokio::pin!(launch);
            loop {
                tokio::select! {
                    launched = &mut launch => break launched,
                    msg = inbox.recv() => {
                        let Some(msg) = msg else {
                            // The launch drops with the stop.
                            let text = messages::acp::SESSION_AGENT_EXITED;
                            self.fail_all(waiting, text).await;
                            return AgentEnd::Stopped;
                        };
                        self.wait(waiting, msg).await;
                    }
                    () = tokio::time::sleep_until(deadline) => {
                        return AgentEnd::StartFailed(StartFailure::from_message(&timeout));
                    }
                }
            }
        };
        let (location, process) = match launched {
            Ok(launched) => launched,
            Err(e) => return self.launch_failed(waiting, &e).await,
        };
        // The arguments can hold secrets. Only the trace level logs them.
        tracing::info!(
            sandbox = location.sandbox,
            guest_cwd = location.guest_cwd,
            cmd = config.argv.first(),
            "agent started"
        );
        tracing::trace!(argv = ?config.argv, "agent command");
        let stderr = pool.stderr.clone();
        let mut agent = Agent::new(location, process, config.max_line_bytes, stderr);
        let init = rpc::request(rpc::INIT_ID, "initialize", &pool.init_params);
        agent.send(&init);
        let mut registering: Registering = None;
        let mut reopen_due = self.reopen_key.is_some();
        loop {
            if agent.init.is_some() && registering.is_none() && reopen_due {
                reopen_due = false;
                if let Some(end) = self.send_reopen(&mut agent, &mut registering).await {
                    return self.end_agent(agent, waiting, end).await;
                }
            }
            while agent.init.is_some()
                && registering.is_none()
                && self.reopen_key.is_none()
                && let Some(msg) = waiting.pop_front()
            {
                if let Some(end) = self.forward(&mut agent, &mut registering, msg).await {
                    return self.end_agent(agent, waiting, end).await;
                }
            }
            tokio::select! {
                msg = inbox.recv() => match msg {
                    Some(msg) => self.wait(waiting, msg).await,
                    None => return self.end_agent(agent, waiting, AgentEnd::Stopped).await,
                },
                registered = registration(&mut registering) => {
                    registering = None;
                    let msg = self.mcp.add(registered);
                    agent.send(&msg);
                }
                item = agent.output.next() => {
                    let end = match item {
                        Some(Output::Line(line)) => self.on_line(&mut agent, &line).await,
                        Some(Output::TooLong) => self.too_long(&mut agent),
                        Some(Output::Exit(code)) => return self.lose(&agent, waiting, code).await,
                        None => return self.lose(&agent, waiting, 1).await,
                    };
                    if let Some(end) = end {
                        return self.end_agent(agent, waiting, end).await;
                    }
                }
                () = tokio::time::sleep_until(deadline), if agent.init.is_none() => {
                    let end = AgentEnd::StartFailed(StartFailure::from_message(&timeout));
                    return self.end_agent(agent, waiting, end).await;
                }
            }
        }
    }

    /// Handle the failed agent launch `error`. A session list in a dir
    /// without a sandbox gets an empty list.
    async fn launch_failed(
        &mut self,
        waiting: &mut VecDeque<Value>,
        error: &anyhow::Error,
    ) -> AgentEnd {
        let list = self.first_method == "session/list";
        // Editors list the sessions of old dirs, for example removed git
        // worktrees. These dirs have no sandbox, thus they have no
        // sessions. An error makes the editor show a failure.
        if !list || error.downcast_ref::<NoSandbox>().is_none() {
            return AgentEnd::StartFailed(StartFailure::from_error(error));
        }
        tracing::debug!("empty session list: {error:#}");
        let empty = json!({ "sessions": [] });
        let ids = request_ids(waiting.drain(..));
        self.answer(ids, |req_id| rpc::result_response(req_id, &empty))
            .await;
        AgentEnd::Stopped
    }

    /// Keep the client message `msg` until the agent can take it. Refuse
    /// it if too many messages wait for the reopen.
    async fn wait(&mut self, waiting: &mut VecDeque<Value>, msg: Value) {
        if self.reopen_key.is_some() && waiting.len() >= MAX_REOPEN_WAITING {
            tracing::debug!("refuse client message: reopen queue full");
            if let Some(Kind::Request { id: req_id, .. }) = rpc::kind(&msg) {
                let error = (rpc::INTERNAL_ERROR, messages::acp::REOPEN_QUEUE_FULL.into());
                self.reply(req_id, error).await;
            }
            return;
        }
        waiting.push_back(msg);
    }

    /// Stop the agent for `end`. If the session ends, the open and waiting
    /// requests get an error.
    /// Returns:
    ///   `end`.
    async fn end_agent(
        &mut self,
        mut agent: Agent,
        waiting: &mut VecDeque<Value>,
        end: AgentEnd,
    ) -> AgentEnd {
        let code = self.stop(&mut agent).await;
        // A one-shot agent stops after its response. Its stop code is not
        // a failure of the session.
        if !self.one_shot {
            self.record(code);
        }
        // The other ends give their own error to the requests.
        if matches!(end, AgentEnd::Stopped) {
            let text = messages::acp::agent_exited(agent.sandbox(), code);
            self.fail_all(waiting, &text).await;
        }
        end
    }

    /// Handle the exit of the agent by itself with the exit code `code`.
    async fn lose(&mut self, agent: &Agent, waiting: &mut VecDeque<Value>, code: i32) -> AgentEnd {
        if agent.init.is_none() {
            let failure = StartFailure::from_message(messages::acp::AGENT_EXITED);
            return AgentEnd::StartFailed(failure);
        }
        self.record(code);
        let sandbox = agent.sandbox();
        let text = messages::acp::agent_exited(sandbox, code);
        if self.reopen_key.is_some() {
            return AgentEnd::ReopenFailed(text);
        }
        self.fail_all(waiting, &text).await;
        if let Some(sid) = self.claimed.clone() {
            let detail = messages::acp::agent_stopped_detail(sandbox, code);
            self.notice(&sid, "error", messages::acp::AGENT_STOPPED, &detail)
                .await;
        }
        AgentEnd::Lost
    }

    /// Stop the agent: close its input, and kill it after the grace time
    /// or at the kill token.
    /// Returns:
    ///   The exit code, 0 if the agent did not complete its handshake, 1
    ///   if it did not exit after the kill.
    async fn stop(&mut self, agent: &mut Agent) -> i32 {
        let grace = self.pool.config.shutdown_grace;
        // An agent without a session has nothing to save. Kill it now.
        if agent.init.is_none() {
            agent.kill();
            self.wait_exit(agent, grace).await;
            return 0;
        }
        let sandbox = agent.sandbox().to_string();
        tracing::info!(sandbox, "stop the agent");
        // The agent exits at the end of its input.
        agent.close_input();
        let mut code = self.wait_exit(agent, grace).await;
        if code.is_none() {
            tracing::info!(sandbox, "kill the agent");
            agent.kill();
            code = self.wait_exit(agent, grace).await;
        }
        code.unwrap_or(1)
    }

    /// Wait for the agent exit, at most `limit`. After the handshake, the
    /// agent output goes to the client in that time. The kill token kills
    /// the agent.
    /// Returns:
    ///   The exit code, or `None` if the agent did not exit in `limit`.
    async fn wait_exit(&mut self, agent: &mut Agent, limit: Duration) -> Option<i32> {
        // An agent without a handshake has no output for the client.
        let relay = agent.init.is_some();
        let deadline = Instant::now() + limit;
        let kill = self.pool.kill.clone();
        let mut killed = false;
        loop {
            tokio::select! {
                item = agent.output.next() => match item {
                    Some(Output::Exit(code)) => return Some(code),
                    None => return Some(1),
                    // The agent stops anyway. Thus the step of a line does
                    // not count.
                    Some(Output::Line(line)) if relay => {
                        let _ = self.on_line(agent, &line).await;
                    }
                    Some(Output::TooLong) if relay => {
                        let _ = self.too_long(agent);
                    }
                    Some(_) => {}
                },
                () = tokio::time::sleep_until(deadline) => return None,
                () = kill.cancelled(), if !killed => {
                    killed = true;
                    agent.kill();
                }
            }
        }
    }

    /// Handle an agent output line that is too long.
    /// Returns:
    ///   The start failure if the handshake did not complete.
    fn too_long(&self, agent: &mut Agent) -> Option<AgentEnd> {
        if agent.limited() {
            return None;
        }
        let message = messages::acp::agent_line_too_long(self.pool.config.max_line_bytes);
        if agent.init.is_none() {
            return Some(AgentEnd::StartFailed(StartFailure::from_message(&message)));
        }
        agent.kill_for_limit(&message);
        None
    }

    /// Send the reopen request to the agent, with the method that the
    /// agent supports.
    /// Returns:
    ///   The reopen failure if the agent cannot reopen the session or the
    ///   proxy refused the request.
    async fn send_reopen(
        &mut self,
        agent: &mut Agent,
        registering: &mut Registering,
    ) -> Option<AgentEnd> {
        let init = agent.init.as_ref().unwrap_or(&Value::Null);
        let Some(method) = reopen_method(init) else {
            let detail = messages::acp::REOPEN_NOT_SUPPORTED.to_string();
            return Some(AgentEnd::ReopenFailed(detail));
        };
        let params = json!({
            "sessionId": self.claimed,
            "cwd": self.cwd,
            "mcpServers": self.mcp_servers,
        });
        let msg = rpc::request(&rpc::reopen_id(self.hid), method, &params);
        // The proxy can refuse the reopen request before it goes out.
        if !self.prepare(agent, registering, msg).await {
            let detail = messages::acp::REOPEN_REFUSED.to_string();
            return Some(AgentEnd::ReopenFailed(detail));
        }
        None
    }

    /// Send the client message `msg` to the agent.
    /// Returns:
    ///   The end of the agent, or `None` if it continues.
    async fn forward(
        &mut self,
        agent: &mut Agent,
        registering: &mut Registering,
        msg: Value,
    ) -> Option<AgentEnd> {
        let first = msg.get("id").map(key).as_ref() == Some(&self.first_key);
        let sent = self.prepare(agent, registering, msg).await;
        // The proxy can refuse the one-shot request before it goes out.
        // The client then has its response.
        (self.one_shot && first && !sent).then_some(AgentEnd::Stopped)
    }

    /// Check the message `msg`, map its session dir to the guest and send
    /// it to the agent. A session request with editor MCP servers first
    /// registers them in `registering`. It goes to the agent after the
    /// registration.
    /// Returns:
    ///   `false` if the request was refused and got an error.
    async fn prepare(
        &mut self,
        agent: &mut Agent,
        registering: &mut Registering,
        mut msg: Value,
    ) -> bool {
        let Some(Kind::Request { id: req_id, method }) = rpc::kind(&msg) else {
            agent.send(&msg);
            return true;
        };
        let (req_id, method) = (req_id.clone(), method.to_string());
        let host_cwd = msg
            .pointer("/params/cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(cwd) = &host_cwd {
            match self.guest_cwd(&agent.location, cwd).await {
                Ok(guest) => {
                    tracing::info!(
                        method,
                        cwd,
                        sandbox = agent.sandbox(),
                        guest,
                        "session request"
                    );
                    msg["params"]["cwd"] = Value::String(guest);
                }
                Err(error) => {
                    tracing::info!(method, cwd, "refuse session request: {}", error.1);
                    self.reply(&req_id, error).await;
                    return false;
                }
            }
        }
        let config = self.pool.config.clone();
        let init = agent.init.as_ref().unwrap_or(&Value::Null);
        let mut no_params = Value::Null;
        let params = msg.get_mut("params").unwrap_or(&mut no_params);
        if let Err(error) = capabilities::check_client_request(&method, params, init) {
            tracing::debug!(method, "refuse client request: {}", error.1);
            self.reply(&req_id, error).await;
            return false;
        }
        if config.proxy_mcp_servers && mcp::SESSION_METHODS.contains(&method.as_str()) {
            let request = mcp::SessionRequest {
                method: &method,
                id: &req_id,
                host_cwd: host_cwd.as_deref(),
                restrict: config.restrict,
                agent_init: init,
                registrar: agent.mcp.clone(),
                timeout: config.mcp_register_timeout,
                sandbox: agent.sandbox(),
            };
            // The request and the later client messages wait for the end
            // of the registration. The line can have env and header
            // values. Thus the log does not show it.
            return match self.mcp.prepare(request, msg) {
                Ok(registration) => {
                    self.open.insert(key(&req_id), (req_id, false));
                    *registering = Some(registration);
                    true
                }
                Err(error) => {
                    tracing::warn!(method, "refuse session request: {}", error.1);
                    self.reply(&req_id, error).await;
                    false
                }
            };
        }
        let removed = mcp::remove_all(params);
        if removed > 0 {
            tracing::info!(
                method,
                removed,
                "remove editor MCP servers: --proxy-mcp-servers is not set"
            );
        }
        if method == "session/close" {
            self.mcp.track_close(&req_id, params);
        }
        let list = method == "session/list";
        self.open.insert(key(&req_id), (req_id, list));
        agent.send(&msg);
        true
    }

    /// Get the guest path of the session dir `cwd`.
    /// Args:
    ///  - `location`: Sandbox of the agent
    ///
    /// Returns:
    ///   The guest path, or the error code and message if `cwd` is not an
    ///   absolute path or is not in the sandbox of the agent.
    async fn guest_cwd(&self, location: &Location, cwd: &str) -> Result<String, (i64, String)> {
        if cwd == self.cwd {
            return Ok(location.guest_cwd.clone());
        }
        if !cwd.starts_with('/') {
            return Err((rpc::INVALID_PARAMS, messages::acp::cwd_not_absolute(cwd)));
        }
        let found = self
            .pool
            .launcher
            .locate(cwd)
            .await
            .map_err(|e| (rpc::INTERNAL_ERROR, format!("{e:#}")))?;
        // One agent runs in one sandbox. It cannot see the files of another
        // sandbox.
        if found.sandbox != location.sandbox {
            return Err((rpc::INVALID_PARAMS, messages::acp::other_sandbox(cwd)));
        }
        Ok(found.guest_cwd)
    }

    /// Handle one line of agent output, without its newline.
    /// Returns:
    ///   The end of the agent, or `None` if it continues.
    async fn on_line(&mut self, agent: &mut Agent, line: &[u8]) -> Option<AgentEnd> {
        // The output of an agent that went over a limit is not for the
        // client.
        if agent.limited() {
            return None;
        }
        let Ok(mut msg) = serde_json::from_slice::<Value>(line) else {
            tracing::debug!("drop agent line that is not JSON");
            return None;
        };
        let mut claim = None;
        let mut done = false;
        if let Some(Kind::Response { id: req_id }) = rpc::kind(&msg) {
            // Only the handshake request of the proxy has this id.
            if agent.init.is_none() && req_id.as_str() == Some(rpc::INIT_ID) {
                return self.on_handshake(agent, &msg).await;
            }
            let req_id = req_id.clone();
            let id_key = key(&req_id);
            let Some((_, list)) = self.open.remove(&id_key) else {
                tracing::debug!(id = %req_id, "drop agent response without a request");
                return None;
            };
            self.mcp.on_response(&req_id, &msg);
            if list {
                let location = &agent.location;
                rpc::map_session_list(&mut msg, &location.host_root, &location.guest_root);
            }
            if self.reopen_key.as_ref() == Some(&id_key) {
                return self.on_reopened(agent, &msg);
            }
            if id_key == self.first_key && self.one_shot {
                let mut listed = self.pool.listed.borrow_mut();
                let sid = self.first_sid.as_deref();
                listed.record(&self.first_method, &self.cwd, sid, &msg);
                done = true;
            } else if id_key == self.first_key && self.claimed.is_none() {
                claim = self.claim(&msg);
            }
        } else if self.one_shot {
            tracing::debug!("drop agent message of a one-shot session");
            return None;
        }
        if self.pool.config.restrict && !allowed(agent, &msg) {
            return None;
        }
        rpc::add_auth_hint(&mut msg, &self.pool.config.name);
        // The client has the history of a session that opens again.
        if self.hides(&msg) {
            return None;
        }
        let claimed = claim.is_some();
        self.to_client(msg, claim).await;
        // The client knows the ACP session only after the response.
        if claimed && let Some(reader) = agent.notices.clone() {
            self.show_notices(reader);
        }
        done.then_some(AgentEnd::Stopped)
    }

    /// Complete the agent handshake with the agent reply `reply`.
    /// Returns:
    ///   The start failure if the agent refused the handshake.
    async fn on_handshake(&mut self, agent: &mut Agent, reply: &Value) -> Option<AgentEnd> {
        match check_handshake(reply, &self.pool.offered, agent.sandbox()) {
            Ok(result) => agent.init = Some(result),
            Err(failure) => return Some(AgentEnd::StartFailed(failure)),
        }
        // The session request, or the reopen request, goes out next. A
        // one-shot session has nothing to save at a signal.
        if !self.one_shot {
            self.send(HandleMessage::Ready).await;
        }
        None
    }

    /// Get the ACP session that the response `msg` to the session request
    /// starts, and keep it.
    /// Returns:
    ///   The session, or `None` if `msg` is an error or has no session.
    fn claim(&mut self, msg: &Value) -> Option<Claim> {
        if msg.get("error").is_some() {
            return None;
        }
        let sid = msg
            .pointer("/result/sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| self.first_sid.clone().filter(|_| self.reload))?;
        self.claimed = Some(sid.clone());
        Some(Claim {
            session: sid,
            replace: self.reload,
        })
    }

    /// Handle the agent response `msg` to the reopen request. The client
    /// does not see it. On success, show the sandbox notices and let the
    /// waiting client messages go.
    /// Returns:
    ///   The reopen failure if the agent refused the reopen.
    fn on_reopened(&mut self, agent: &Agent, msg: &Value) -> Option<AgentEnd> {
        if let Some(err) = msg.get("error") {
            let detail = err
                .get("message")
                .and_then(Value::as_str)
                .map_or_else(|| err.to_string(), str::to_string);
            return Some(AgentEnd::ReopenFailed(detail));
        }
        tracing::info!(sandbox = agent.sandbox(), "session reopened");
        self.reopen_key = None;
        if let Some(reader) = agent.notices.clone() {
            self.show_notices(reader);
        }
        None
    }

    /// Check if the agent message `msg` is history of the ACP session that
    /// opens again. The client has it already.
    fn hides(&self, msg: &Value) -> bool {
        self.reopen_key.is_some()
            && matches!(
                rpc::kind(msg),
                Some(Kind::Notification {
                    method: "session/update"
                })
            )
            && session_id(msg).is_some_and(|sid| self.claimed.as_deref() == Some(sid))
    }

    /// Wait for the next client request of a session whose agent is gone.
    /// A close gets its reply at once. Other messages are dropped.
    /// Returns:
    ///   The request, or `None` if the proxy stopped the session.
    async fn next_request(&mut self, inbox: &mut Inbox) -> Option<Value> {
        loop {
            let msg = inbox.recv().await?;
            match rpc::kind(&msg) {
                // The agent is gone. Nothing is left to close.
                Some(Kind::Request {
                    id: req_id,
                    method: "session/close",
                }) => {
                    let msg = rpc::result_response(req_id, &json!({}));
                    self.to_client(msg, None).await;
                }
                Some(Kind::Request { .. }) => return Some(msg),
                _ => tracing::debug!("drop client message: the agent is gone"),
            }
        }
    }

    /// Make the next agent open the ACP session again before the client
    /// messages.
    fn start_reopen(&mut self) {
        tracing::info!(sid = ?self.claimed, "reopen the session");
        self.reopen_key = Some(key(&Value::from(rpc::reopen_id(self.hid))));
    }

    /// Check if `req_id` is the reopen request that has no response yet.
    fn is_reopen(&self, req_id: &Value) -> bool {
        self.reopen_key.as_ref() == Some(&key(req_id))
    }

    /// Keep the exit code `code` if it is the first that is not 0.
    fn record(&mut self, code: i32) {
        if self.code == 0 {
            self.code = code;
        }
    }

    /// Give each request in `waiting` the error of the failed agent start
    /// `failure`, and log the first request.
    async fn fail_start(&mut self, waiting: &mut VecDeque<Value>, failure: &StartFailure) {
        // Log the first request to show which client call wants the
        // sandbox. Cut long params, for example prompt text.
        let first = waiting
            .iter()
            .find(|m| m.get("id").map(key).as_ref() == Some(&self.first_key));
        let mut params = first
            .and_then(|m| m.get("params"))
            .map(Value::to_string)
            .unwrap_or_default();
        if let Some((cut, _)) = params.char_indices().nth(MAX_LOGGED_PARAMS) {
            params.truncate(cut);
            params.push('…');
        }
        tracing::warn!(
            method = self.first_method,
            params,
            "agent start failed: {}",
            failure.message
        );
        let ids = request_ids(waiting.drain(..));
        self.answer(ids, |req_id| failure.response(req_id)).await;
    }

    /// Give the open and waiting requests the error of the failed reopen
    /// with `detail`.
    async fn fail_reopen(&mut self, waiting: &mut VecDeque<Value>, detail: &str) {
        tracing::warn!("session reopen failed: {detail}");
        let sid = self.claimed.clone().unwrap_or_default();
        let text = messages::acp::session_reopen_failed(sid, detail);
        self.fail_all(waiting, &text).await;
        self.reopen_key = None;
    }

    /// Give each open and waiting request the error `text`. Drop the other
    /// waiting messages.
    async fn fail_all(&mut self, waiting: &mut VecDeque<Value>, text: &str) {
        let open = std::mem::take(&mut self.open)
            .into_values()
            .map(|(id, _)| id);
        let ids: Vec<Value> = open.chain(request_ids(waiting.drain(..))).collect();
        self.answer(ids, |req_id| {
            rpc::error_response(req_id, rpc::INTERNAL_ERROR, text)
        })
        .await;
    }

    /// Send the response that `response` makes to each request in `ids`.
    /// The reopen request gets no response, because the client did not
    /// send it.
    async fn answer(&self, ids: Vec<Value>, response: impl Fn(&Value) -> Value) {
        for req_id in ids {
            if !self.is_reopen(&req_id) {
                self.to_client(response(&req_id), None).await;
            }
        }
    }

    /// Release the editor MCP servers of the session, with a time limit.
    async fn release_mcp(&mut self) {
        let release = self.mcp.release_all();
        if tokio::time::timeout(self.pool.config.shutdown_grace, release)
            .await
            .is_err()
        {
            tracing::info!("MCP server release did not complete");
        }
    }

    /// End the session: refuse the late client messages, release the MCP
    /// servers, and tell the proxy.
    /// Returns:
    ///   The exit code of the session.
    async fn finish(mut self, mut inbox: Inbox) -> i32 {
        // The handle takes no more messages. The ones that came before get
        // an error, else the client waits for ever.
        inbox.close();
        let late = request_ids(std::iter::from_fn(|| inbox.try_recv().ok()));
        let text = messages::acp::SESSION_AGENT_EXITED;
        self.answer(late, |req_id| {
            rpc::error_response(req_id, rpc::INTERNAL_ERROR, text)
        })
        .await;
        self.release_mcp().await;
        self.send(HandleMessage::Ended).await;
        self.code
    }

    /// Tell the client about all notices of the sandbox, one warning for
    /// each, in the ACP session of the session request. Does nothing if
    /// the session has no ACP session.
    /// Args:
    ///  - `reader`: Reader of the sandbox notices
    fn show_notices(&self, reader: NoticeReader) {
        let Some(sid) = self.claimed.clone() else {
            return;
        };
        let (hid, tx, notices) = (self.hid, self.pool.messages.clone(), self.pool.notices);
        // The read goes to the sandbox. The agent output must not wait for
        // it. The warnings come after the response, because the response
        // is in the queue before the read starts.
        let show = async move {
            for notice in reader.read().await {
                let msg = notice_message(
                    notices,
                    &sid,
                    "warning",
                    messages::acp::SANDBOX_NOTICE,
                    &notice.text,
                );
                let claim = None;
                let _ = tx.send((hid, HandleMessage::Client { msg, claim })).await;
            }
        };
        tokio::task::spawn_local(show.in_current_span());
    }

    /// Send a message to the client in the ACP session `sid` (see
    /// [`notice_message`]).
    async fn notice(&self, sid: &str, severity: &str, title: &str, detail: &str) {
        let msg = notice_message(self.pool.notices, sid, severity, title, detail);
        self.to_client(msg, None).await;
    }

    /// Send the error `error` for the client request `req_id`. The client
    /// does not get the error for the reopen request.
    async fn reply(&self, req_id: &Value, error: (i64, String)) {
        if self.is_reopen(req_id) {
            tracing::info!("refuse reopen request: {}", error.1);
            return;
        }
        let msg = rpc::error_response(req_id, error.0, &error.1);
        self.to_client(msg, None).await;
    }

    /// Send a message to the client through the proxy.
    async fn to_client(&self, msg: Value, claim: Option<Claim>) {
        self.send(HandleMessage::Client { msg, claim }).await;
    }

    /// Send a message about the session to the proxy.
    async fn send(&self, message: HandleMessage) {
        let _ = self.pool.messages.send((self.hid, message)).await;
    }
}

/// Apply the restrict rules to the agent message `msg`. Refuse an agent
/// request that the rules do not let through.
/// Returns:
///   `true` if the message goes to the client.
// Only one JSON object at a time goes to the client, as the proxy
// encodes it. Thus the client cannot read a message that the proxy did
// not check, for example in a batch.
fn allowed(agent: &mut Agent, msg: &Value) -> bool {
    if !msg.is_object() {
        tracing::debug!("drop agent line that is not one object");
        return false;
    }
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("-");
    match capabilities::check_agent_message(msg) {
        AgentVerdict::Forward => true,
        AgentVerdict::Refuse { code, message } => {
            tracing::debug!(method, "refuse agent request: {message}");
            let req_id = msg.get("id").cloned().unwrap_or(Value::Null);
            agent.send(&rpc::error_response(&req_id, code, &message));
            false
        }
        AgentVerdict::Drop => {
            tracing::debug!(method, "drop agent message");
            false
        }
    }
}

/// Get the ids of the requests in the client messages `msgs`.
fn request_ids(msgs: impl Iterator<Item = Value>) -> Vec<Value> {
    msgs.filter_map(|msg| match rpc::kind(&msg) {
        Some(Kind::Request { id: req_id, .. }) => Some(req_id.clone()),
        _ => None,
    })
    .collect()
}

/// Wait for the end of the MCP server registration. Waits forever if no
/// registration runs.
async fn registration(registering: &mut Registering) -> mcp::Registered {
    match registering {
        Some(registration) => registration.as_mut().await,
        None => std::future::pending().await,
    }
}

/// Make a message to the client in the ACP session `sid`: a session notice
/// with `severity` if the client shows notices, else agent text.
/// Args:
///  - `notices`: The client shows session notices
///  - `title`, `detail`: Title and text of the message
fn notice_message(notices: bool, sid: &str, severity: &str, title: &str, detail: &str) -> Value {
    let update = if notices {
        json!({
            "sessionUpdate": "notice", "severity": severity,
            "title": title, "description": detail,
        })
    } else {
        json!({
            "sessionUpdate": "agent_message_chunk",
            "content": { "type": "text", "text": format!("{title}: {detail}") },
        })
    };
    let params = json!({ "sessionId": sid, "update": update });
    json!({ "jsonrpc": "2.0", "method": "session/update", "params": params })
}
