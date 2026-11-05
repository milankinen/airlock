//! One agent process for all sessions in one sandbox.
//!
//! Launches the agent in the sandbox and does the one agent handshake with
//! the client params. Then gives the agent the session requests and the
//! later client messages of all its sessions, with guest session dirs.
//! Client messages that come before the handshake wait, in order. Applies
//! the message rules to both directions.
//!
//! Each agent message goes to one session. A response goes to the session
//! of its request. A message of an ACP session goes to the session that has
//! it. A new ACP session id, for example of a sub agent, goes to the
//! session with the last open prompt, else to the oldest session. A message
//! without an ACP session goes to the oldest session. No agent message
//! goes to a one-shot session, only the response to its request.
//!
//! Each session shows all notices of the sandbox as warnings, after the OK
//! response to its session request. A session whose session request fails
//! and a one-shot session show none.
//!
//! A one-shot session ends by itself after its response. When the agent
//! exits, all its sessions end. When the last session ends, the agent input
//! closes. An agent that does not exit in the grace time is killed.
//!
//! The memory use has a limit. An agent that writes a too long line or
//! does not read its input is stopped. A slow client slows the agent. The
//! agent stderr never stops the agent. When the host stderr is slow, the
//! agent stderr is dropped.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::rc::Rc;
use std::task::Poll;

use futures::future::LocalBoxFuture;
use serde_json::{Value, json};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

use super::super::capabilities::{self, AgentVerdict};
use super::super::proxy::{Claim, HandleId, HandleMessage};
use super::super::rpc::{self, Kind, SessionId, key};
use super::super::{AgentProcess, AgentProcessLauncher, Control, Location, mcp};
use super::Pool;
use super::session::Session;
use crate::messages;
use crate::rpc::ProcessEvent;
use crate::sandbox::ExitedBeforeReady;

/// Request from a session handle to the shared agent task.
pub(super) enum Command {
    /// Add a session with its session request.
    Attach {
        hid: HandleId,
        location: Location,
        cwd: String,
        first: Value,
    },
    /// A client message of a session.
    Client { hid: HandleId, msg: Value },
    /// End a session. `done` gets the exit code: 0, or the exit code of the
    /// agent if the session was the last one.
    Detach {
        hid: HandleId,
        done: oneshot::Sender<i32>,
    },
}

/// Lifetime of a shared agent process, as its handles see it.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Phase {
    /// The agent takes new sessions.
    Open,
    /// The agent stops or failed. It takes no new sessions.
    Closed,
    /// The agent task ended with this exit code.
    Ended(i32),
}

/// Handle side of a shared agent task.
pub(super) struct Link {
    commands: mpsc::UnboundedSender<Command>,
    phase: Cell<Phase>,
    /// The sessions that attached and did not detach. A one-shot session
    /// that ended by itself is not in the set, also before its handle
    /// detaches.
    attached: RefCell<HashSet<HandleId>>,
}

impl Link {
    /// Check if the agent takes new sessions.
    pub(super) fn accepting(&self) -> bool {
        self.phase.get() == Phase::Open
    }

    /// Take no new sessions.
    fn close(&self) {
        if self.phase.get() == Phase::Open {
            self.phase.set(Phase::Closed);
        }
    }

    /// Get the exit code of the agent, after the task end.
    fn exit_code(&self) -> Option<i32> {
        match self.phase.get() {
            Phase::Ended(code) => Some(code),
            Phase::Open | Phase::Closed => None,
        }
    }

    /// Get the number of the attached sessions.
    fn sessions(&self) -> usize {
        self.attached.borrow().len()
    }

    /// Add the session `hid` with its session request `first` and the
    /// client messages `queued` that came before.
    pub(super) fn attach(
        &self,
        hid: HandleId,
        location: Location,
        cwd: String,
        first: Value,
        queued: Vec<Value>,
    ) {
        self.attached.borrow_mut().insert(hid);
        let attach = Command::Attach {
            hid,
            location,
            cwd,
            first,
        };
        let _ = self.commands.send(attach);
        for msg in queued {
            self.send(hid, msg);
        }
    }

    /// Remove the session `hid` from the attached sessions.
    /// Returns:
    ///   The number of the sessions that are left.
    fn release(&self, hid: HandleId) -> usize {
        let mut attached = self.attached.borrow_mut();
        attached.remove(&hid);
        attached.len()
    }

    /// Give the client message `msg` of the session `hid` to the agent.
    /// Returns:
    ///   `false` if the session ended or the agent task ended.
    pub(super) fn send(&self, hid: HandleId, msg: Value) -> bool {
        if !self.attached.borrow().contains(&hid) {
            return false;
        }
        self.commands.send(Command::Client { hid, msg }).is_ok()
    }

    /// End the session `hid`.
    /// Returns:
    ///   A future of the exit code: 0, or the exit code of the agent if the
    ///   session was the last one or the agent exited.
    pub(super) fn detach(self: &Rc<Self>, hid: HandleId) -> LocalBoxFuture<'static, i32> {
        // A one-shot session that ended by itself is not attached. The
        // task removes the other sessions when it handles the detach.
        if !self.attached.borrow().contains(&hid) {
            return Box::pin(std::future::ready(0));
        }
        if let Some(code) = self.exit_code() {
            return Box::pin(std::future::ready(code));
        }
        let (done, code) = oneshot::channel();
        let sent = self.commands.send(Command::Detach { hid, done }).is_ok();
        let link = self.clone();
        Box::pin(async move {
            let code = if sent { code.await.ok() } else { None };
            code.or(link.exit_code()).unwrap_or(0)
        })
    }
}

/// Start the shared agent task for the sandbox `sandbox`.
/// Args:
///  - `pool`: Settings of the agents
///  - `sandbox`: Identity of the sandbox
///  - `cwd`: Host session dir of the first session, for the launch
///
/// Returns:
///   The handle side of the task.
pub(super) fn spawn<S: AgentProcessLauncher + 'static>(
    pool: Rc<Pool<S>>,
    sandbox: String,
    cwd: String,
) -> Rc<Link> {
    let (commands, inbox) = mpsc::unbounded_channel();
    let link = Rc::new(Link {
        commands,
        phase: Cell::new(Phase::Open),
        attached: RefCell::default(),
    });
    tracing::info!(sandbox, cwd, "launch an agent");
    let agent = SharedAgent {
        pool,
        link: link.clone(),
        cwd,
        pipe: Pipe {
            sandbox,
            process: None,
            stopping: false,
            partial: Vec::new(),
        },
        handshake: None,
        ending: None,
        sessions: BTreeMap::new(),
        owners: HashMap::new(),
        next_prompt: 0,
    };
    tokio::task::spawn_local(agent.run(inbox));
    link
}

/// Line I/O of the agent process.
struct Pipe {
    /// Sandbox of the agent.
    sandbox: String,
    process: Option<AgentProcess>,
    /// The agent was killed for a limit. Its output and input are dropped
    /// until it exits.
    stopping: bool,
    /// Stdout bytes after the last complete line. They have no newline.
    partial: Vec<u8>,
}

impl Pipe {
    /// Send a control request to the agent, if it runs.
    fn control(&self, request: Control) {
        if let Some(process) = &self.process {
            let _ = process.control.send(request);
        }
    }

    /// Kill the agent for a limit, and drop its later output.
    fn kill_for_limit(&mut self, message: &str) {
        tracing::warn!(sandbox = self.sandbox, "stop the agent: {message}");
        self.control(Control::Kill);
        self.stopping = true;
        self.partial = Vec::new();
    }

    /// Send a message to the agent, if it runs. Kills the agent if its
    /// input queue is full.
    fn send(&mut self, msg: &Value) {
        let Some(process) = &self.process else {
            return;
        };
        if self.stopping {
            return;
        }
        let line = rpc::to_line(msg);
        if tracing::enabled!(tracing::Level::TRACE) {
            trace(
                &self.sandbox,
                "airlock",
                &rpc::to_line(&hide_mcp_secrets(msg)),
            );
        }
        // The agent must read its input. A full queue thus stops the
        // agent, so that the proxy does not keep lines without limit.
        if let Err(TrySendError::Full(_)) = process.input.try_send(line) {
            self.kill_for_limit(messages::acp::AGENT_INPUT_FULL);
        }
    }
}

/// Launch result of an agent process.
type Launched = anyhow::Result<(Location, AgentProcess)>;

/// State of one shared agent task.
struct SharedAgent<S> {
    pool: Rc<Pool<S>>,
    link: Rc<Link>,
    /// Host session dir of the first session.
    cwd: String,
    pipe: Pipe,
    /// `initialize` result of the agent, after the handshake.
    handshake: Option<Value>,
    /// Why the task ends, when it ends.
    ending: Option<Ending>,
    /// The attached sessions, the oldest first.
    sessions: BTreeMap<HandleId, Session>,
    /// Session of each known ACP session id.
    owners: HashMap<SessionId, HandleId>,
    /// Order number of the next prompt.
    next_prompt: u64,
}

/// Why a shared agent task ends.
enum Ending {
    /// The agent exited or stopped with this exit code.
    Done(i32),
    /// The start or the handshake failed with this error. The waiting
    /// session requests get it. The exit code is 0.
    Failed(StartFailure),
    /// The last session was one-shot and ended. The agent stops.
    Idle,
}

impl<S: AgentProcessLauncher + 'static> SharedAgent<S> {
    /// Run the agent until it exits or its last session ends. Then end all
    /// sessions.
    async fn run(mut self, mut inbox: mpsc::UnboundedReceiver<Command>) {
        let code = self.serve(&mut inbox).await;
        self.link.phase.set(Phase::Ended(code));
        self.pool.forget(&self.pipe.sandbox, &self.link);
        // The commands that came after the end get an error too.
        inbox.close();
        while let Ok(command) = inbox.try_recv() {
            self.on_late(command);
        }
        let sandbox = self.pipe.sandbox.clone();
        let error = messages::acp::agent_exited(&sandbox, code);
        let mut releases = Vec::new();
        for session in self.sessions.values_mut() {
            let notify = session.detach.is_none().then_some((sandbox.as_str(), code));
            let failed = match &self.ending {
                Some(Ending::Failed(failure)) => Some(failure),
                _ => None,
            };
            session.end(&error, failed, notify).await;
            releases.push(session.mcp.release_all());
        }
        let releases = futures::future::join_all(releases);
        if tokio::time::timeout(self.pool.config.shutdown_grace, releases)
            .await
            .is_err()
        {
            tracing::info!(sandbox, "MCP server release did not complete");
        }
        for (_, mut session) in std::mem::take(&mut self.sessions) {
            session.send(HandleMessage::Ended).await;
            if let Some(done) = session.detach.take() {
                let _ = done.send(code);
            }
        }
    }

    /// Take no new sessions.
    fn close(&self) {
        self.link.close();
    }

    /// Check if the agent exited, stopped or failed. The last one-shot
    /// session end does not count. It stops the agent after the current
    /// step.
    fn ended(&self) -> bool {
        matches!(self.ending, Some(Ending::Done(_) | Ending::Failed(_)))
    }

    /// Launch the agent and relay its messages.
    /// Returns:
    ///   The exit code, 0 if the agent did not complete its handshake.
    async fn serve(&mut self, inbox: &mut mpsc::UnboundedReceiver<Command>) -> i32 {
        let config = self.pool.config.clone();
        let deadline = Instant::now() + config.handshake_timeout;
        let timeout = messages::acp::handshake_timeout(config.handshake_timeout.as_secs());
        let pool = self.pool.clone();
        let cwd = self.cwd.clone();
        let mut launching: Option<LocalBoxFuture<'static, Launched>> = Some(Box::pin(async move {
            pool.launcher.launch(&cwd, &pool.config.argv).await
        }));
        loop {
            let ready = self.handshake.is_some();
            tokio::select! {
                launched = next_launch(&mut launching) => {
                    launching = None;
                    self.on_launched(launched);
                }
                command = inbox.recv() => match command {
                    Some(command) => self.on_command(command).await,
                    // All handles are gone.
                    None => return self.stop().await,
                },
                (hid, registered) = next_registration(&mut self.sessions) => {
                    self.on_registered(hid, registered).await;
                }
                event = next_event(&mut self.pipe.process) => self.on_event(event).await,
                () = tokio::time::sleep_until(deadline), if !ready => {
                    self.fail_start(&timeout);
                }
            }
            match self.ending {
                Some(Ending::Done(code)) => return code,
                Some(Ending::Failed(_)) => return 0,
                Some(Ending::Idle) => return self.stop().await,
                None => {}
            }
        }
    }

    /// Start the handshake of the launched agent, or fail the start.
    fn on_launched(&mut self, launched: Launched) {
        let (location, process) = match launched {
            Ok(launched) => launched,
            Err(e) => {
                self.fail_start_with(StartFailure::from_error(&e));
                return;
            }
        };
        let config = &self.pool.config;
        // The arguments can hold secrets. Only the trace level logs them.
        tracing::info!(
            sandbox = location.sandbox,
            guest_cwd = location.guest_cwd,
            cmd = config.argv.first(),
            "agent started"
        );
        tracing::trace!(argv = ?config.argv, "agent command");
        self.pipe.process = Some(process);
        let init = rpc::request(rpc::INIT_ID, "initialize", &self.pool.init_params);
        self.pipe.send(&init);
    }

    /// Handle one command of a session handle.
    async fn on_command(&mut self, command: Command) {
        match command {
            Command::Attach {
                hid,
                location,
                cwd,
                first,
            } => {
                tracing::info!(
                    handle = hid,
                    sandbox = self.pipe.sandbox,
                    "attach a session"
                );
                let session = self.new_session(hid, location, cwd, first);
                self.sessions.insert(hid, session);
                self.drain(hid).await;
            }
            Command::Client { hid, msg } => {
                let Some(session) = self.sessions.get_mut(&hid) else {
                    tracing::debug!(handle = hid, "drop client message of an ended session");
                    return;
                };
                session.queue.push_back(msg);
                self.drain(hid).await;
            }
            Command::Detach { hid, done } => self.detach(hid, done).await,
        }
    }

    /// Handle a command that came after the end of the agent.
    fn on_late(&mut self, command: Command) {
        match command {
            Command::Attach {
                hid,
                location,
                cwd,
                first,
            } => {
                let session = self.new_session(hid, location, cwd, first);
                self.sessions.insert(hid, session);
            }
            Command::Client { hid, msg } => {
                if let Some(session) = self.sessions.get_mut(&hid) {
                    session.queue.push_back(msg);
                }
            }
            Command::Detach { hid, done } => match self.sessions.get_mut(&hid) {
                Some(session) => session.detach = Some(done),
                None => {
                    let _ = done.send(self.link.exit_code().unwrap_or(0));
                }
            },
        }
    }

    /// Make the session `hid` of the session request `first`.
    fn new_session(&self, hid: HandleId, location: Location, cwd: String, first: Value) -> Session {
        let messages = self.pool.messages.clone();
        Session::new(hid, location, cwd, first, messages, self.pool.notices)
    }

    /// End the session `hid`. The last session stops the agent.
    async fn detach(&mut self, hid: HandleId, done: oneshot::Sender<i32>) {
        let left = self.link.release(hid);
        if left == 0 {
            // The session stays until the agent exits. Its last output
            // thus still gets to the client.
            self.close();
            match self.sessions.get_mut(&hid) {
                Some(session) => session.detach = Some(done),
                None => drop(done),
            }
            let code = self.stop().await;
            self.ending = Some(Ending::Done(code));
            return;
        }
        let Some(mut session) = self.sessions.remove(&hid) else {
            let _ = done.send(0);
            return;
        };
        tracing::info!(
            handle = hid,
            sandbox = self.pipe.sandbox,
            "detach a session"
        );
        self.owners.retain(|_, owner| *owner != hid);
        session
            .end(messages::acp::SESSION_DETACHED, None, None)
            .await;
        let release = session.mcp.release_all();
        let grace = self.pool.config.shutdown_grace;
        let sandbox = self.pipe.sandbox.clone();
        // The other sessions must not wait for the release.
        tokio::task::spawn_local(async move {
            if tokio::time::timeout(grace, release).await.is_err() {
                tracing::info!(sandbox, "MCP server release did not complete");
            }
            session.send(HandleMessage::Ended).await;
            let _ = done.send(0);
        });
    }

    /// Send the waiting client messages of the session `hid` to the agent,
    /// until the next MCP registration. Nothing goes before the handshake.
    async fn drain(&mut self, hid: HandleId) {
        if self.handshake.is_none() {
            return;
        }
        loop {
            if self.ended() {
                return;
            }
            let Some(session) = self.sessions.get_mut(&hid) else {
                return;
            };
            if session.registration.is_some() {
                return;
            }
            let Some(msg) = session.queue.pop_front() else {
                return;
            };
            if !session.first_sent {
                session.first_sent = true;
                // A one-shot session has nothing to save at a signal.
                if !session.one_shot {
                    session.send(HandleMessage::Ready).await;
                }
            }
            self.on_client(hid, msg).await;
            // The agent can refuse the request before it goes out.
            self.end_one_shot(hid).await;
        }
    }

    /// End the one-shot session `hid` if it has its response. The last
    /// session stops the agent.
    async fn end_one_shot(&mut self, hid: HandleId) {
        // A session that the proxy ends gets the normal detach.
        let done = self
            .sessions
            .get(&hid)
            .is_some_and(|s| s.one_shot_done() && s.detach.is_none());
        if !done {
            return;
        }
        let Some(mut session) = self.sessions.remove(&hid) else {
            return;
        };
        tracing::info!(
            handle = hid,
            sandbox = self.pipe.sandbox,
            "end a one-shot session"
        );
        let left = self.link.release(hid);
        self.owners.retain(|_, owner| *owner != hid);
        session
            .end(messages::acp::SESSION_DETACHED, None, None)
            .await;
        session.send(HandleMessage::Ended).await;
        if left == 0 {
            self.close();
            self.ending.get_or_insert(Ending::Idle);
        }
    }

    /// Give the request with the registered MCP servers to the agent. Then
    /// send the client messages that waited.
    async fn on_registered(&mut self, hid: HandleId, registered: mcp::Registered) {
        let Some(session) = self.sessions.get_mut(&hid) else {
            return;
        };
        session.registration = None;
        let msg = session.mcp.add(registered);
        self.pipe.send(&msg);
        self.drain(hid).await;
    }

    /// Stop the agent when its last session ends: close its input, and
    /// kill it after the grace time or at the kill token.
    /// Returns:
    ///   The exit code, 0 if the agent did not complete its handshake, 1
    ///   if it did not exit after the kill.
    async fn stop(&mut self) -> i32 {
        self.close();
        let grace = self.pool.config.shutdown_grace;
        // An agent without a session has nothing to save. Kill it now.
        if self.handshake.is_none() {
            if self.pipe.process.is_some() {
                self.pipe.control(Control::Kill);
                self.wait_exit(grace).await;
            }
            return 0;
        }
        tracing::info!(sandbox = self.pipe.sandbox, "stop the agent");
        if let Some(process) = &mut self.pipe.process {
            // The agent exits at the end of its input.
            let (closed, _) = mpsc::channel(1);
            drop(std::mem::replace(&mut process.input, closed));
        }
        let mut code = self.wait_exit(grace).await;
        if code.is_none() {
            tracing::info!(sandbox = self.pipe.sandbox, "kill the agent");
            self.pipe.control(Control::Kill);
            code = self.wait_exit(grace).await;
        }
        code.unwrap_or(1)
    }

    /// Wait for the agent exit and relay its output in that time. The kill
    /// token kills the agent.
    /// Returns:
    ///   The exit code, or `None` if the agent did not exit in `limit`.
    async fn wait_exit(&mut self, limit: std::time::Duration) -> Option<i32> {
        let deadline = Instant::now() + limit;
        let kill = self.pool.kill.clone();
        let mut killed = false;
        loop {
            tokio::select! {
                event = next_event(&mut self.pipe.process) => {
                    if let ProcessEvent::Exit(code) = event {
                        tracing::info!(sandbox = self.pipe.sandbox, code, "agent exited");
                        self.flush_partial().await;
                        return Some(code);
                    }
                    self.on_event(event).await;
                }
                () = tokio::time::sleep_until(deadline) => return None,
                () = kill.cancelled(), if !killed => {
                    killed = true;
                    self.pipe.control(Control::Kill);
                }
            }
        }
    }

    /// Handle one agent event.
    async fn on_event(&mut self, event: ProcessEvent) {
        match event {
            ProcessEvent::Stdout(data) => self.on_stdout(&data).await,
            ProcessEvent::Stderr(data) => self.relay_stderr(data),
            ProcessEvent::Exit(code) => {
                tracing::info!(sandbox = self.pipe.sandbox, code, "agent exited");
                self.flush_partial().await;
                self.close();
                if self.handshake.is_none() {
                    self.fail_start(messages::acp::AGENT_EXITED);
                } else {
                    self.ending = Some(Ending::Done(code));
                }
            }
        }
    }

    /// Split the agent output into lines and handle each complete line.
    async fn on_stdout(&mut self, data: &[u8]) {
        if self.pipe.stopping {
            return;
        }
        let max = self.pool.config.max_line_bytes;
        let start = self.pipe.partial.len();
        self.pipe.partial.extend_from_slice(data);
        // The partial line has no newline. Thus only the new bytes need a
        // search, and the time stays linear for long lines.
        if let Some(pos) = data.iter().rposition(|&b| b == b'\n') {
            let rest = self.pipe.partial.split_off(start + pos + 1);
            let complete = std::mem::replace(&mut self.pipe.partial, rest);
            for line in complete[..complete.len() - 1].split(|&b| b == b'\n') {
                if line.len() > max {
                    self.stop_for_limit(&messages::acp::agent_line_too_long(max));
                    return;
                }
                self.on_agent_line(line).await;
                if self.pipe.stopping || self.ended() {
                    return;
                }
            }
        }
        // A line without its end must not fill the memory.
        if self.pipe.partial.len() > max {
            self.stop_for_limit(&messages::acp::agent_line_too_long(max));
        }
    }

    /// Handle the last agent output line when it has no newline.
    async fn flush_partial(&mut self) {
        if !self.pipe.stopping && !self.pipe.partial.is_empty() {
            let line = std::mem::take(&mut self.pipe.partial);
            self.on_agent_line(&line).await;
        }
    }

    /// Send agent stderr to the host stderr. Drop it if the stderr queue is
    /// full.
    // The agent must not wait for the host stderr. An editor that does not
    // read it would stop the relay.
    fn relay_stderr(&self, data: Vec<u8>) {
        let stderr = &self.pool.stderr;
        let len = data.len() as u64;
        if let Err(TrySendError::Full(_)) = stderr.tx.try_send(data) {
            if stderr.dropped.get() == 0 {
                tracing::warn!(
                    sandbox = self.pipe.sandbox,
                    "drop agent stderr: stderr is full"
                );
            }
            stderr.dropped.set(stderr.dropped.get() + len);
        }
    }

    /// Handle one line of agent output, without its newline.
    async fn on_agent_line(&mut self, line: &[u8]) {
        let sandbox = self.pipe.sandbox.as_str();
        trace(sandbox, "agent", line);
        let Ok(mut msg) = serde_json::from_slice::<Value>(line) else {
            tracing::debug!(sandbox, "drop agent line that is not JSON");
            return;
        };
        let mut claim = None;
        let mut target = None;
        if let Some(Kind::Response { id: req_id }) = rpc::kind(&msg) {
            // Only the handshake request of the proxy has this id.
            if self.handshake.is_none() && req_id.as_str() == Some(rpc::INIT_ID) {
                self.on_handshake(&msg).await;
                return;
            }
            let req_id = req_id.clone();
            let Some((hid, started)) = self.on_response(&req_id, &mut msg) else {
                return;
            };
            (target, claim) = (Some(hid), started);
        }
        if self.pool.config.restrict && !self.allowed(&msg) {
            return;
        }
        rpc::add_auth_hint(&mut msg, &self.pool.config.name);
        let Some(hid) = target.or_else(|| self.route(&msg)) else {
            tracing::debug!(
                sandbox = self.pipe.sandbox,
                "drop agent message: no session"
            );
            return;
        };
        if let Some(session) = self.sessions.get(&hid) {
            let claimed = claim.is_some();
            session.to_client(msg, claim).await;
            // The client knows the ACP session only after the response.
            let reader = self.pipe.process.as_ref().and_then(|p| p.notices.clone());
            if claimed && let Some(reader) = reader {
                session.show_notices(reader);
            }
        }
        self.end_one_shot(hid).await;
    }

    /// Give the agent response `msg` to the session of its request `req_id`.
    /// Returns:
    ///   The session, and the ACP session that the response starts. `None`
    ///   if no session has the request.
    fn on_response(
        &mut self,
        req_id: &Value,
        msg: &mut Value,
    ) -> Option<(HandleId, Option<Claim>)> {
        let id_key = key(req_id);
        let owner = self
            .sessions
            .values_mut()
            .find(|s| s.open.contains_key(&id_key));
        let Some(session) = owner else {
            let sandbox = self.pipe.sandbox.as_str();
            tracing::debug!(sandbox, id = %req_id, "drop agent response without a request");
            return None;
        };
        let claim = session.on_response(req_id, msg);
        let mut listed = self.pool.listed.borrow_mut();
        // The agent stops after a one-shot response if it has no other
        // session. A new session must then not attach to it.
        if session.on_one_shot_response(req_id, msg, &mut listed) && self.link.sessions() == 1 {
            self.link.close();
        }
        if let Some(claim) = &claim {
            self.owners.insert(claim.session.clone(), session.hid);
        }
        Some((session.hid, claim))
    }

    /// Apply the restrict rules to the agent message `msg`. Refuse an agent
    /// request that the rules do not let through.
    /// Returns:
    ///   `true` if the message goes to the client.
    // Only one JSON object at a time goes to the client, as the proxy
    // encodes it. Thus the client cannot read a message that the proxy did
    // not check, for example in a batch.
    fn allowed(&mut self, msg: &Value) -> bool {
        let sandbox = self.pipe.sandbox.as_str();
        if !msg.is_object() {
            tracing::debug!(sandbox, "drop agent line that is not one object");
            return false;
        }
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("-");
        match capabilities::check_agent_message(msg) {
            AgentVerdict::Forward => true,
            AgentVerdict::Refuse { code, message } => {
                tracing::debug!(sandbox, method, "refuse agent request: {message}");
                let req_id = msg.get("id").cloned().unwrap_or(Value::Null);
                self.pipe
                    .send(&rpc::error_response(&req_id, code, &message));
                false
            }
            AgentVerdict::Drop => {
                tracing::debug!(sandbox, method, "drop agent message");
                false
            }
        }
    }

    /// Find the session of an agent message that is not a response. A new
    /// ACP session id becomes a session of the found session.
    // All sessions are in the same sandbox. A wrong guess thus shows a
    // message in the wrong session of the editor, but does not cross a
    // sandbox.
    fn route(&mut self, msg: &Value) -> Option<HandleId> {
        let oldest = self.sessions.values().find(|s| !s.one_shot).map(|s| s.hid);
        let Some(sid) = rpc::session_id(msg) else {
            return oldest;
        };
        if let Some(&hid) = self.owners.get(sid) {
            return Some(hid);
        }
        let prompting = self
            .sessions
            .values()
            .filter(|s| !s.one_shot)
            .filter_map(|s| s.last_prompt().map(|order| (order, s.hid)))
            .max()
            .map(|(_, hid)| hid);
        let hid = prompting.or(oldest)?;
        tracing::debug!(handle = hid, sid, "give a new ACP session to a session");
        self.owners.insert(sid.to_string(), hid);
        Some(hid)
    }

    /// Complete the agent handshake with the agent reply `reply`. Then send
    /// the waiting session requests to the agent.
    async fn on_handshake(&mut self, reply: &Value) {
        if let Some(err) = reply.get("error") {
            self.fail_start(&messages::acp::agent_handshake_failed(err));
            return;
        }
        let result = reply.get("result").cloned().unwrap_or(Value::Null);
        let version = result.get("protocolVersion").unwrap_or(&Value::Null);
        if version.as_u64() != Some(rpc::PROTOCOL_VERSION) {
            let message =
                messages::acp::unsupported_protocol_version(version, rpc::PROTOCOL_VERSION);
            self.fail_start(&message);
            return;
        }
        // The log helps to keep the presets true to the real agents.
        let missing = capabilities::missing(&self.pool.offered, &result);
        if !missing.is_empty() {
            tracing::info!(
                sandbox = self.pipe.sandbox,
                ?missing,
                "the agent does not have offered features"
            );
        }
        tracing::info!(sandbox = self.pipe.sandbox, "agent handshake complete");
        self.handshake = Some(result);
        let waiting: Vec<HandleId> = self.sessions.keys().copied().collect();
        for hid in waiting {
            self.drain(hid).await;
        }
    }

    /// Handle one client message of the session `hid`: check it, map the
    /// session dir to the guest and send it to the agent.
    async fn on_client(&mut self, hid: HandleId, mut msg: Value) {
        // The proxy sends only the messages of the own ACP sessions. A
        // one-shot session owns no ACP sessions.
        let owner = self.sessions.get(&hid).is_some_and(|s| !s.one_shot);
        if let Some(sid) = rpc::session_id(&msg)
            && owner
        {
            self.owners.insert(sid.to_string(), hid);
        }
        let Some(Kind::Request { id: req_id, method }) = rpc::kind(&msg) else {
            self.pipe.send(&msg);
            return;
        };
        let (req_id, method) = (req_id.clone(), method.to_string());
        let Some(session) = self.sessions.get_mut(&hid) else {
            return;
        };
        let config = &self.pool.config;
        let host_cwd = msg
            .pointer("/params/cwd")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(cwd) = &host_cwd {
            match session.guest_cwd(&self.pool.launcher, cwd).await {
                Ok(guest) => {
                    tracing::info!(
                        method,
                        cwd,
                        sandbox = self.pipe.sandbox,
                        guest,
                        "session request"
                    );
                    msg["params"]["cwd"] = Value::String(guest);
                }
                Err(error) => {
                    tracing::info!(method, cwd, "refuse session request: {}", error.1);
                    session.reply(&req_id, error).await;
                    return;
                }
            }
        }
        let handshake = self.handshake.as_ref().unwrap_or(&Value::Null);
        let mut no_params = Value::Null;
        let params = msg.get_mut("params").unwrap_or(&mut no_params);
        if let Err(error) = capabilities::check_client_request(&method, params, handshake) {
            tracing::debug!(method, "refuse client request: {}", error.1);
            session.reply(&req_id, error).await;
            return;
        }
        if config.proxy_mcp_servers && mcp::SESSION_METHODS.contains(&method.as_str()) {
            let request = mcp::SessionRequest {
                method: &method,
                id: &req_id,
                host_cwd: host_cwd.as_deref(),
                restrict: config.restrict,
                agent_init: handshake,
                registrar: self.pipe.process.as_ref().and_then(|p| p.mcp.clone()),
                timeout: config.mcp_register_timeout,
                sandbox: &self.pipe.sandbox,
            };
            // The request and the later client messages of the session
            // wait for the end of the registration. The line can have env
            // and header values. Thus the log does not show it.
            match session.mcp.prepare(request, msg) {
                Ok(registration) => {
                    session.open.insert(key(&req_id), req_id);
                    session.registration = Some(registration);
                }
                Err(error) => {
                    tracing::warn!(method, "refuse session request: {}", error.1);
                    session.reply(&req_id, error).await;
                }
            }
            return;
        }
        let removed = mcp::remove_all(params);
        if removed > 0 {
            tracing::info!(
                method,
                removed,
                "remove editor MCP servers: --proxy-mcp-servers is not set"
            );
        }
        session.track(req_id, &method, params, &mut self.next_prompt);
        self.pipe.send(&msg);
    }

    /// Kill the agent because it went over a limit. Before the handshake
    /// completes, fail the start with `message`.
    fn stop_for_limit(&mut self, message: &str) {
        if self.handshake.is_none() {
            self.fail_start(message);
            return;
        }
        self.close();
        self.pipe.kill_for_limit(message);
    }

    /// Stop the agent that did not complete its start or handshake. The
    /// waiting session requests get `message` at the task end.
    fn fail_start(&mut self, message: &str) {
        self.fail_start_with(StartFailure {
            message: message.to_string(),
            data: None,
        });
    }

    /// Stop the agent that did not complete its start (see
    /// [`Self::fail_start`]). The waiting session requests get `failure`.
    fn fail_start_with(&mut self, failure: StartFailure) {
        let message = &failure.message;
        tracing::warn!(sandbox = self.pipe.sandbox, "agent start failed: {message}");
        self.close();
        self.pipe.control(Control::Kill);
        self.ending = Some(Ending::Failed(failure));
    }
}

/// Error of a failed agent start, for the waiting session requests.
pub(super) struct StartFailure {
    /// Error message.
    message: String,
    /// Error data, or `None` if the error has none.
    data: Option<Value>,
}

impl StartFailure {
    /// Get the failure of the agent launch error `error`. A sandbox process
    /// that stopped in its init gives the last line of its error output as
    /// the message, and its exit code and output as the data. The editor
    /// can then show why the sandbox did not start.
    fn from_error(error: &anyhow::Error) -> Self {
        let Some(exited) = error.downcast_ref::<ExitedBeforeReady>() else {
            return Self {
                message: format!("{error:#}"),
                data: None,
            };
        };
        let message = exited
            .last_error
            .clone()
            .unwrap_or_else(|| format!("{error:#}"));
        let data = json!({
            "exitCode": exited.code,
            "output": exited.output.clone().unwrap_or_default(),
        });
        Self {
            message,
            data: Some(data),
        }
    }

    /// Make the error response to the client request `req_id`.
    pub(super) fn response(&self, req_id: &Value) -> Value {
        match &self.data {
            Some(data) => {
                rpc::error_response_with_data(req_id, rpc::INTERNAL_ERROR, &self.message, data)
            }
            None => rpc::error_response(req_id, rpc::INTERNAL_ERROR, &self.message),
        }
    }
}

/// Log a message at the trace level, as `<from>: <line>`, with the
/// sandbox as a field.
fn trace(sandbox: &str, from: &str, line: &[u8]) {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    tracing::trace!(sandbox, "{from}: {}", String::from_utf8_lossy(line));
}

/// Text that replaces a secret value in the log.
const HIDDEN: &str = "***";

/// Get a copy of `msg` where the env and header values of each editor MCP
/// server are [`HIDDEN`].
fn hide_mcp_secrets(msg: &Value) -> Value {
    let mut msg = msg.clone();
    let Some(servers) = msg
        .pointer_mut("/params/mcpServers")
        .and_then(Value::as_array_mut)
    else {
        return msg;
    };
    for server in servers {
        for field in ["env", "headers"] {
            match server.get_mut(field) {
                Some(Value::Array(items)) => {
                    for value in items.iter_mut().filter_map(|i| i.get_mut("value")) {
                        *value = HIDDEN.into();
                    }
                }
                // Not valid ACP, but the values can still be secrets.
                Some(Value::Object(map)) => map.values_mut().for_each(|v| *v = HIDDEN.into()),
                _ => {}
            }
        }
    }
    msg
}

/// Wait for the end of the agent launch. Waits forever if no launch runs.
async fn next_launch(launching: &mut Option<LocalBoxFuture<'static, Launched>>) -> Launched {
    match launching {
        Some(launching) => launching.await,
        None => std::future::pending().await,
    }
}

/// Wait for the end of the next MCP server registration of a session.
/// Waits forever if no registration runs.
async fn next_registration(
    sessions: &mut BTreeMap<HandleId, Session>,
) -> (HandleId, mcp::Registered) {
    std::future::poll_fn(|cx| {
        for (hid, session) in sessions.iter_mut() {
            if let Some(registration) = &mut session.registration
                && let Poll::Ready(registered) = registration.as_mut().poll(cx)
            {
                return Poll::Ready((*hid, registered));
            }
        }
        Poll::Pending
    })
    .await
}

/// Wait for the next agent event. Waits forever if no agent runs.
async fn next_event(process: &mut Option<AgentProcess>) -> ProcessEvent {
    let Some(process) = process else {
        return std::future::pending().await;
    };
    // A relay that ends without the exit event lost the agent.
    process.events.recv().await.unwrap_or(ProcessEvent::Exit(1))
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
        let hidden = hide_mcp_secrets(&request);
        assert!(!hidden.to_string().contains("secret"), "{hidden}");
        let servers = &hidden["params"]["mcpServers"];
        assert_eq!(
            servers[0]["env"][0],
            json!({"name": "API_KEY", "value": HIDDEN})
        );
        assert_eq!(servers[1]["headers"][0]["name"], "Authorization");
    }
}
