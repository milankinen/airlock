//! One running MCP server process.
//!
//! Starts the server program and relays its stdout lines to the guest in
//! order. Writes the guest messages to its stdin. Sends the program's
//! stderr to the log. Tells the guest when the process stops, after the
//! last message. Stops the process and all processes that it started when
//! the guest releases the session, or when the host stops the session.
//!
//! Keeps the time of the last message in either direction. Thus the host
//! can stop a session that is idle for too long. Other kinds of sessions
//! use the same activity and stop state.

use std::cell::{Cell, RefCell};
use std::process::Stdio;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::supervisor_capnp::{mcp_session, mcp_sink};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStderr, ChildStdin, ChildStdout};
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::ServerSpec;
use crate::messages;
use crate::util::VerifiedPath;
use crate::util::process::{exited_unreaped, kill_group, tie_to_airlock};

/// Largest stdout line (one JSON-RPC message) in bytes. A longer line
/// stops the session, so that a bad server cannot make the host keep
/// data without limit.
const MAX_LINE: usize = 16 * 1024 * 1024;

/// Largest stderr line in bytes. A longer line goes to the log in parts.
const MAX_LOG_LINE: usize = 64 * 1024;

/// Number of guest messages that can wait for the server to read them.
/// When the queue is full, `send` fails. Thus a server that does not read
/// its stdin cannot block the guest.
const SEND_QUEUE: usize = 64;

/// Time that a server has to stop on its own after it closes its stdout.
/// After that, the session stops it.
const EOF_GRACE: Duration = Duration::from_secs(1);

/// Time that the guest has after the session stop to take a server
/// message and the close. A guest that does not answer must not block the
/// end of the session.
const SINK_GRACE: Duration = Duration::from_secs(1);

/// Interval of the extra exit checks, in case a child signal is lost.
const EXIT_POLL: Duration = Duration::from_secs(1);

/// Shared state of one session: activity, the stop signal and the close
/// reason. Each kind of session keeps one. The host uses it for the
/// session limit and to stop the session.
pub(crate) struct SessionState {
    /// Tells the wait task to stop the server. It also fires when the
    /// server stops for another reason.
    stop: CancellationToken,
    /// Time of the last message in either direction.
    last_active: Cell<Instant>,
    /// Close reason when the host stops the session.
    reason: RefCell<Option<String>>,
}

impl SessionState {
    /// Make the state of a new session. The session is active now.
    pub(crate) fn new() -> Self {
        Self {
            stop: CancellationToken::new(),
            last_active: Cell::new(Instant::now()),
            reason: RefCell::default(),
        }
    }

    /// Time of the last message in either direction.
    pub(crate) fn last_active(&self) -> Instant {
        self.last_active.get()
    }

    /// Check if the session stopped or got the stop signal.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stop.is_cancelled()
    }

    /// Stop the session. The guest gets `reason` as the close reason,
    /// unless the session already stopped. The wait task fires `stop`
    /// when the server exits. Thus a stop after the exit keeps the exit
    /// status as the reason.
    pub(crate) fn stop_with(&self, reason: String) {
        if self.stop.is_cancelled() {
            return;
        }
        self.reason.borrow_mut().get_or_insert(reason);
        self.stop.cancel();
    }

    /// Record a message in either direction.
    pub(crate) fn touch(&self) {
        self.last_active.set(Instant::now());
    }

    /// Signal that fires when the session stops. A cancel of the signal
    /// stops the session without a reason.
    pub(crate) fn stop_token(&self) -> CancellationToken {
        self.stop.clone()
    }

    /// Take the close reason that a stop gave, if any.
    pub(crate) fn take_reason(&self) -> Option<String> {
        self.reason.borrow_mut().take()
    }
}

/// Cap'n Proto `McpSession` server. It owns the stdin of one server
/// process. Drop stops the process group.
pub(super) struct McpSessionImpl {
    /// Queue of the stdin writer task. Each item is one full line.
    stdin: mpsc::Sender<Vec<u8>>,
    /// Process ID of the server. It is also its process group ID.
    pid: u32,
    /// Set when the wait task reaps the server. After that, the group ID
    /// can belong to an unrelated group.
    reaped: Rc<Cell<bool>>,
    /// Activity and stop signal, shared with the tasks and the host.
    state: Rc<SessionState>,
}

impl McpSessionImpl {
    /// Shared state of the session, for the session limit of the host.
    pub(super) fn state(&self) -> Rc<SessionState> {
        self.state.clone()
    }
}

/// Program of a stdio server: the parts of its transport that start the
/// process.
pub(super) struct StdioProgram<'a> {
    /// Program to run.
    pub(super) command: &'a str,
    /// Arguments of the program.
    pub(super) args: &'a [String],
    /// Environment values, on top of the host environment.
    pub(super) env: &'a [(String, String)],
}

/// Start a process of the server `spec` and its relay tasks. Call inside
/// a `LocalSet`.
/// Args:
///  - `spec`: Server to start, for its name and idle timeout
///  - `program`: Program of the server, from the transport of `spec`
///  - `cwd`: Working directory of the process
///  - `sink`: Guest receiver of the server messages
///  - `on_end`: Called one time after the `closed` call to the guest
///
/// Returns:
///   Session of the new process, or error if the process does not start.
pub(super) fn start(
    spec: &ServerSpec,
    program: &StdioProgram<'_>,
    cwd: &VerifiedPath,
    sink: mcp_sink::Client,
    on_end: impl FnOnce() + 'static,
) -> Result<McpSessionImpl, capnp::Error> {
    // Check the project dir again at each start. A component can be a
    // symlink now.
    let cwd = cwd.to_path().map_err(|e| {
        capnp::Error::failed(messages::mcp::start_failed(&spec.name, program.command, e))
    })?;
    let mut cmd = tokio::process::Command::new(program.command);
    cmd.args(program.args)
        .envs(program.env.iter().map(|(k, v)| (k, v)))
        .current_dir(cwd)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Last resort: if the runtime stops before the wait task runs,
        // the drop of the child still kills it.
        .kill_on_drop(true);
    tie_to_airlock(cmd.as_std_mut());
    let mut child = cmd.spawn().map_err(|e| {
        capnp::Error::failed(messages::mcp::start_failed(&spec.name, program.command, e))
    })?;
    // A child that is not reaped yet always has an ID.
    let pid = child.id().expect("pid of running child");
    let (Some(stdin), Some(stdout), Some(stderr)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        unreachable!("piped stdio");
    };
    tracing::debug!("mcp {}: started pid {pid}", spec.name);

    let reaped = Rc::new(Cell::new(false));
    let state = Rc::new(SessionState::new());
    let stop = state.stop.clone();
    let (exit_tx, exit_rx) = oneshot::channel();
    let (stdin_tx, stdin_rx) = mpsc::channel(SEND_QUEUE);
    tokio::task::spawn_local(write_stdin(stdin, stdin_rx));
    tokio::task::spawn_local(drain_stderr(spec.name.clone(), stderr));
    tokio::task::spawn_local(wait_exit(child, pid, reaped.clone(), stop.clone(), exit_tx));
    let relay = relay_stdout(stdout, sink, state.clone(), exit_rx);
    tokio::task::spawn_local(async move {
        relay.await;
        on_end();
    });
    if let Some(timeout) = spec.timeout {
        tokio::task::spawn_local(stop_when_idle(state.clone(), timeout));
    }
    Ok(McpSessionImpl {
        stdin: stdin_tx,
        pid,
        reaped,
        state,
    })
}

impl mcp_session::Server for McpSessionImpl {
    async fn send(
        self: Rc<Self>,
        params: mcp_session::SendParams,
        _results: mcp_session::SendResults,
    ) -> Result<(), capnp::Error> {
        let message = params.get()?.get_message()?.to_str()?;
        // A line break would split the message in two for the server.
        if message.contains(['\n', '\r']) {
            return Err(capnp::Error::failed(
                messages::mcp::MESSAGE_HAS_NEWLINE.into(),
            ));
        }
        let mut line = Vec::with_capacity(message.len() + 1);
        line.extend_from_slice(message.as_bytes());
        line.push(b'\n');
        // Do not wait for room in the queue: a server that does not read
        // its stdin must not block the guest.
        self.stdin.try_send(line).map_err(|e| {
            capnp::Error::failed(match e {
                mpsc::error::TrySendError::Full(_) => messages::mcp::SEND_QUEUE_FULL.to_string(),
                mpsc::error::TrySendError::Closed(_) => messages::mcp::STDIN_CLOSED.to_string(),
            })
        })?;
        self.state.touch();
        Ok(())
    }
}

/// Stop the session when no message went in either direction for
/// `timeout`.
pub(super) async fn stop_when_idle(state: Rc<SessionState>, timeout: Duration) {
    loop {
        let deadline = state.last_active() + timeout;
        if deadline <= Instant::now() {
            state.stop_with(messages::mcp::idle_timeout(timeout.as_secs()));
            return;
        }
        tokio::select! {
            () = tokio::time::sleep_until(deadline) => {}
            () = state.stop.cancelled() => return,
        }
    }
}

/// Wait until [`SINK_GRACE`] after the stop signal `stop`. A sink call
/// that has no answer at that time stops.
pub(super) async fn sink_grace_over(stop: CancellationToken) {
    stop.cancelled().await;
    tokio::time::sleep(SINK_GRACE).await;
}

/// Tell the guest sink `sink` that the session ended with `reason`. Waits
/// at most [`SINK_GRACE`] for the answer.
pub(super) async fn send_closed(sink: &mcp_sink::Client, reason: &str) {
    let mut request = sink.closed_request();
    request.get().set_reason(reason);
    match tokio::time::timeout(SINK_GRACE, request.send().promise).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::debug!("mcp sink closed: {e}"),
        Err(_) => tracing::debug!("mcp sink closed: no answer"),
    }
}

/// Write each queued line to the server stdin, in order. Stops at the
/// first write error, or when the session is released.
async fn write_stdin(mut stdin: ChildStdin, mut lines: mpsc::Receiver<Vec<u8>>) {
    while let Some(line) = lines.recv().await {
        let written = async {
            stdin.write_all(&line).await?;
            stdin.flush().await
        }
        .await;
        if let Err(e) = written {
            tracing::debug!("{}", messages::mcp::send_failed(e));
            return;
        }
    }
}

impl Drop for McpSessionImpl {
    fn drop(&mut self) {
        // Kill the group now: the wait task does not run again if the
        // runtime stops. Before the reap, the group ID cannot go to a new
        // process.
        if !self.reaped.get() {
            kill_group(self.pid);
        }
        self.state.stop.cancel();
    }
}

/// Wait until the server stops, or stop it when `stop` fires. Then stop
/// the rest of its process group, reap it and send the close reason to
/// `exit`.
async fn wait_exit(
    mut child: Child,
    pid: u32,
    reaped: Rc<Cell<bool>>,
    stop: CancellationToken,
    exit: oneshot::Sender<String>,
) {
    // Do not reap the server before the group kill: until the reap, its
    // process ID is also the group ID and cannot go to a new process. Thus
    // the kill cannot reach an unrelated group.
    let mut sigchld = signal(SignalKind::child())
        .inspect_err(|e| tracing::debug!("mcp pid {pid}: SIGCHLD: {e}"))
        .ok();
    let mut poll = tokio::time::interval(EXIT_POLL);
    while !stop.is_cancelled() && !exited_unreaped(pid) {
        tokio::select! {
            Some(()) = async {
                match &mut sigchld {
                    Some(s) => s.recv().await,
                    None => std::future::pending().await,
                }
            } => {}
            _ = poll.tick() => {}
            () = stop.cancelled() => {}
        }
    }
    // Mark the session as stopped. A later idle or limit stop then keeps
    // the exit status as the close reason.
    stop.cancel();
    // Stop the server, or what it left in its group.
    kill_group(pid);
    let status = child.wait().await;
    reaped.set(true);
    let reason = match status {
        Ok(status) => messages::mcp::exited(status),
        Err(e) => {
            tracing::debug!("mcp pid {pid}: wait: {e}");
            messages::mcp::EXIT_UNKNOWN.to_string()
        }
    };
    // The relay task is gone only if the guest is gone.
    let _ = exit.send(reason);
}

/// Send each stdout line of the server to `sink`, in order. At the end of
/// the stdout, stop the server (if it still runs) and send `closed` with
/// the reason from `exit`, or the read problem.
async fn relay_stdout(
    stdout: ChildStdout,
    sink: mcp_sink::Client,
    state: Rc<SessionState>,
    exit: oneshot::Receiver<String>,
) {
    let stop = state.stop.clone();
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    let grace_over = sink_grace_over(stop.clone());
    tokio::pin!(grace_over);
    let problem = loop {
        match read_line(&mut reader, &mut line, MAX_LINE).await {
            Ok(Line::Complete) => {}
            Ok(Line::Eof) => break None,
            Ok(Line::TooLong) => break Some(messages::mcp::line_too_long(MAX_LINE)),
            Err(e) => break Some(messages::mcp::read_failed(e)),
        }
        let text = String::from_utf8_lossy(&line);
        let text = text.strip_suffix('\r').unwrap_or(&text);
        if text.trim().is_empty() {
            continue;
        }
        state.touch();
        let mut request = sink.message_request();
        request.get().set_message(text);
        // Wait for each call. This keeps the order, and a slow guest slows
        // the server output instead of filling host memory. After the stop,
        // the wait has a time limit.
        let sent = tokio::select! {
            sent = request.send().promise => sent,
            () = &mut grace_over => {
                tracing::debug!("mcp sink: no answer after the stop");
                break None;
            }
        };
        if let Err(e) = sent {
            // The guest cannot get messages any more. Thus the session has
            // no use.
            tracing::debug!("mcp sink: {e}");
            stop.cancel();
            return;
        }
    };
    if let Some(problem) = problem {
        state.stop_with(problem);
    }
    // A server that closed its stdout can send nothing more. Give it a
    // short time to stop on its own, so that its own exit status is the
    // reason. Then stop it.
    let mut exit = exit;
    let exited = tokio::select! {
        exited = &mut exit => exited,
        () = tokio::time::sleep(EOF_GRACE) => {
            stop.cancel();
            exit.await
        }
    }
    .unwrap_or_else(|_| messages::mcp::EXIT_UNKNOWN.to_string());
    // The session ended. This also tells the host to forget it.
    stop.cancel();
    let reason = state.reason.borrow_mut().take().unwrap_or(exited);
    send_closed(&sink, &reason).await;
}

/// Write each stderr line of the server to the log.
async fn drain_stderr(name: String, stderr: ChildStderr) {
    let mut reader = BufReader::new(stderr);
    let mut line = Vec::new();
    loop {
        match read_line(&mut reader, &mut line, MAX_LOG_LINE).await {
            Ok(Line::Complete | Line::TooLong) => {
                let text = crate::util::strip_controls(&line);
                tracing::debug!(target: "mcp", "[{name}] {text}");
            }
            Ok(Line::Eof) => return,
            Err(e) => {
                tracing::debug!("mcp {name} stderr: {e}");
                return;
            }
        }
    }
}

/// Result of [`read_line`].
#[derive(Debug, PartialEq, Eq)]
enum Line {
    /// A full line, or the last part of the stream without a line break.
    Complete,
    /// The first `max` bytes of a longer line. The next read continues
    /// the same line.
    TooLong,
    /// End of the stream.
    Eof,
}

/// Read one line from `reader` into `line`, without the line break. Keeps
/// at most `max` bytes, so that the memory use has a limit.
async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    line: &mut Vec<u8>,
    max: usize,
) -> std::io::Result<Line> {
    line.clear();
    loop {
        let chunk = reader.fill_buf().await?;
        if chunk.is_empty() {
            return Ok(if line.is_empty() {
                Line::Eof
            } else {
                Line::Complete
            });
        }
        let (part, used) = match chunk.iter().position(|&b| b == b'\n') {
            Some(i) => (&chunk[..i], i + 1),
            None => (chunk, chunk.len()),
        };
        let room = max - line.len();
        if part.len() > room {
            line.extend_from_slice(&part[..room]);
            reader.consume(room);
            return Ok(Line::TooLong);
        }
        let complete = used > part.len();
        line.extend_from_slice(part);
        reader.consume(used);
        if complete {
            return Ok(Line::Complete);
        }
    }
}
