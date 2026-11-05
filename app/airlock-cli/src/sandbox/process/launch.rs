//! Start of the sandbox process.
//!
//! Starts the sandbox process and gives it the held sandbox locks. Thus no
//! other process can take the sandbox between the two processes. Sends the
//! init data and a second socket for the CLI service, and waits until the
//! sandbox is ready. The sandbox process shares the terminal during its
//! init, so this process does not use the terminal and gives Ctrl+C to the
//! sandbox process: the sandbox process decides. Then attaches to the
//! CLI service on the second socket, lets the sandbox process detach and
//! waits until it does not use the terminal any more.
//!
//! The output of the sandbox process can also come to this process through
//! pipes, for a command whose standard output is a protocol. Then the
//! output goes to the log of this process, and the last part of it comes
//! with the error of a failed init.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::unix::io::AsRawFd;
use std::os::unix::net::UnixStream;
use std::process::{ExitStatus, Stdio};
use std::rc::Rc;
use std::time::Duration;

use futures::FutureExt;
use tokio::io::{AsyncRead, AsyncReadExt, BufReader};
use tokio::net::unix::OwnedReadHalf;
use tokio::signal::unix::{Signal, SignalKind, signal};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use super::init::{
    Init, LogOpts, MESSAGE_CAP, Mode, READY_CAP, Ready, ToChild, ToParent, read_message,
    write_message,
};
use crate::sandbox::Interrupted;
use crate::sandbox::lock::HeldLock;
use crate::util::{self, VerifiedPath};
use crate::{cli, messages};

/// What the sandbox process starts with.
pub struct LaunchRequest {
    /// Registry id of the sandbox.
    pub sandbox_id: String,
    /// Working directory of the sandbox process: the project directory.
    /// The pack configs of the start work then see the same directory as
    /// the user.
    pub cwd: VerifiedPath,
    /// Log and output options.
    pub log: LogOpts,
    /// What the sandbox process does in its init.
    pub mode: Mode,
    /// Send the output of the sandbox process through pipes to the log of
    /// this process, and give it no input. With `false`, the
    /// sandbox process uses the standard streams of this process.
    pub capture_output: bool,
}

/// Size limit of the kept output of a sandbox process.
const CAPTURE_CAP: usize = 64 * 1024;

/// Time limit for the end of the output pipes after the exit or the detach
/// of the sandbox process. A child process of the sandbox process can keep
/// a copy of a pipe.
const CAPTURE_END_TIMEOUT: Duration = Duration::from_secs(1);

/// Time limit for the exit of a sandbox process after the connect step
/// failed. A sandbox process that stops gives its exit code in this time.
const EXIT_GRACE: Duration = Duration::from_millis(500);

/// A sandbox process that detached. A background task collects its exit
/// status.
#[derive(Debug)]
pub struct SandboxProcessChild {
    /// Process id of the sandbox process.
    pub pid: u32,
    /// Exit code of the sandbox process when it stops. `None` if the wait
    /// fails.
    pub exit: oneshot::Receiver<Option<i32>>,
}

/// A started sandbox process with the connection of this process.
pub struct Launched<T> {
    /// The value of the connect step.
    pub connected: T,
    /// The ready message of the sandbox process.
    pub ready: Ready,
    /// The detached sandbox process.
    pub child: SandboxProcessChild,
}

/// Error when the sandbox process stops before the sandbox is ready. The
/// sandbox process gave the reason on its error output.
#[derive(Debug, thiserror::Error)]
#[error("{}", messages::sandbox_process::exited_before_ready(.code))]
pub struct ExitedBeforeReady {
    /// Exit code of the sandbox process. A stop by a signal gives 128 + the
    /// signal number.
    pub code: i32,
    /// The last part of the output, if the request captures the output.
    pub output: Option<String>,
    /// The last line of the error output that is not empty, if the request
    /// captures the output.
    pub last_error: Option<String>,
}

impl ExitedBeforeReady {
    /// Get the exit code of the sandbox process if `error` is an
    /// [`ExitedBeforeReady`].
    pub fn code_of(error: &anyhow::Error) -> Option<i32> {
        error.downcast_ref::<Self>().map(|exited| exited.code)
    }
}

/// Get the program and the arguments of the sandbox process: the binary of
/// this process with `internal vmm`.
/// Returns:
///   The program and its arguments, or error if the binary is not found.
pub fn sandbox_process_program() -> anyhow::Result<(VerifiedPath, [OsString; 2])> {
    // The binary of this process, with its links resolved, as for the
    // update.
    let exe = std::env::current_exe()
        .and_then(|exe| VerifiedPath::new_follow(exe, None))
        .map_err(|e| anyhow::anyhow!(messages::cli::cannot_find_binary(e)))?;
    Ok((exe, crate::cli::cmd_internal::SANDBOX_PROCESS_ARGS.map(Into::into)))
}

/// Start `program` as the sandbox process, wait until its sandbox is
/// ready, connect and let the sandbox process detach.
/// Args:
///  - `program`: The program to start (see [`sandbox_process_program`]).
///    Tests give another program
///  - `args`: Its arguments. The descriptor number of the init socket
///    comes after them. The descriptor of the RPC socket is in the init
///    data
///  - `run`: The held run lock of the sandbox. The sandbox process keeps it
///  - `startup`: The held startup lock. The sandbox process frees it at the
///    detach
///  - `request`: What the sandbox process starts with
///  - `connect`: Attaches to the CLI service after the sandbox is ready. It
///    gets the socket of the first client connection of the sandbox
///
/// Returns:
///   The started sandbox process. Error [`ExitedBeforeReady`] if the init
///   fails, [`Interrupted`] after a Ctrl+C before the start, the error of
///   `connect`, or error if the process does not start.
pub(crate) async fn spawn_program<T>(
    program: &VerifiedPath,
    args: &[OsString],
    run: HeldLock,
    startup: HeldLock,
    request: &LaunchRequest,
    connect: impl AsyncFnOnce(UnixStream) -> anyhow::Result<T>,
) -> anyhow::Result<Launched<T>> {
    if cli::is_interrupted() {
        return Err(Interrupted.into());
    }
    let mut signals = InitSignals::new()?;
    let (parent_end, child_end) = util::fd::socket_pair()?;
    let (parent_rpc, child_rpc) = util::fd::socket_pair()?;
    let init_fd = child_end.as_raw_fd();
    let init = Init {
        sandbox_id: request.sandbox_id.clone(),
        run_lock_fd: run.as_raw_fd(),
        startup_lock_fd: startup.as_raw_fd(),
        rpc_fd: child_rpc.as_raw_fd(),
        log: request.log.clone(),
        mode: request.mode.clone(),
    };
    let mut cmd = tokio::process::Command::new(program.to_path()?);
    cmd.args(args)
        .arg(init_fd.to_string())
        .current_dir(request.cwd.to_path()?);
    if request.capture_output {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    }
    util::fd::inherit_fds(
        &mut cmd,
        vec![init_fd, init.rpc_fd, init.run_lock_fd, init.startup_lock_fd],
    );
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow::anyhow!(messages::sandbox_process::spawn_failed(e)))?;
    // The sandbox process holds the locks on the same open files, and has
    // its own copies of the sockets. Thus the close of these copies does
    // not free the locks, and the end of the streams comes from the sandbox
    // process only.
    drop((child_end, child_rpc, run, startup));
    let pid = child.id().unwrap_or_default();
    // Read the pipes from the start. A sandbox process that writes much
    // output must not block on a full pipe.
    let capture = match (child.stdout.take(), child.stderr.take()) {
        (Some(stdout), Some(stderr)) => Some(Capture::start(stdout, stderr)),
        _ => None,
    };

    // A drop of this future closes the socket. The sandbox process then
    // stops by itself.
    parent_end.set_nonblocking(true)?;
    let (read, mut write) = tokio::net::UnixStream::from_std(parent_end)?.into_split();
    let mut reader = BufReader::new(read);
    // A sandbox process that stopped closed its end. The read below then
    // gives the end of the stream.
    if let Err(e) = write_message(&mut write, &ToChild::Init(init)).await {
        tracing::info!("send the init data: {e:#}");
    }
    let Some(ready) = wait_ready(&mut reader, pid, &mut signals).await else {
        // The sandbox process can wait for the detach after a ready
        // message that this process refused. The end of the streams stops
        // it.
        drop((write, reader, parent_rpc));
        let status = wait_exit(&mut child, pid, &mut signals).await;
        return Err(exited_before_ready(status, capture).await);
    };
    let connected = match connect(parent_rpc).await {
        Ok(connected) => connected,
        Err(e) => {
            // A sandbox process that stops, for example after a Ctrl+C,
            // closes the RPC socket. Its exit code then tells the reason.
            let exit = tokio::time::timeout(EXIT_GRACE, wait_exit(&mut child, pid, &mut signals));
            if let Ok(status) = exit.await {
                return Err(exited_before_ready(status, capture).await);
            }
            // The end of the stream before the detach stops the sandbox
            // process. Wait for it, so that its output does not mix with the
            // next output.
            drop((write, reader));
            wait_exit(&mut child, pid, &mut signals).await;
            return Err(e);
        }
    };
    if let Err(e) = write_message(&mut write, &ToChild::Detach).await {
        tracing::info!("send the detach message: {e:#}");
    }
    // The sandbox process tells that it detached and closes its end after
    // it left the terminal. A sandbox process that stops closes its end
    // with no message.
    let mut detached = false;
    loop {
        tokio::select! {
            message = read_message::<ToParent>(&mut reader, MESSAGE_CAP) => match message {
                Ok(Some(ToParent::Detached)) => {
                    detached = true;
                    signals.set_detached();
                }
                Ok(Some(_)) => {}
                _ => break,
            },
            signal = signals.next() => signals.handle(signal, pid),
        }
    }
    if !detached {
        drop(connected);
        let status = wait_exit(&mut child, pid, &mut signals).await;
        return Err(exited_before_ready(status, capture).await);
    }
    // The sandbox process detached its output, so the pipes end now. The
    // last lines go to the log.
    if let Some(capture) = capture {
        capture.finish().await;
    }
    signals.finish();

    let (exit_tx, exit) = oneshot::channel();
    tokio::spawn(async move {
        let code = child.wait().await.ok().map(exit_code);
        let _ = exit_tx.send(code);
    });
    Ok(Launched {
        connected,
        ready,
        child: SandboxProcessChild { pid, exit },
    })
}

/// Make the error for a sandbox process that stopped before its detach.
/// Args:
///  - `status`: The exit status of the sandbox process
///  - `capture`: The captured output, if the request captures the output
async fn exited_before_ready(
    status: Option<ExitStatus>,
    capture: Option<Capture>,
) -> anyhow::Error {
    let (output, last_error) = match capture {
        Some(capture) => {
            let (output, last_error) = capture.finish().await;
            (Some(output), last_error)
        }
        None => (None, None),
    };
    ExitedBeforeReady {
        code: exit_code(status),
        output,
        last_error,
    }
    .into()
}

/// Output of a sandbox process from its pipes. Copies the output to the
/// log of this process and keeps the last [`CAPTURE_CAP`] bytes.
struct Capture {
    kept: Rc<RefCell<VecDeque<u8>>>,
    last_error: Rc<RefCell<Option<String>>>,
    tasks: Vec<JoinHandle<()>>,
}

impl Capture {
    /// Start to read `stdout` and `stderr` in local tasks. Needs a
    /// `LocalSet`: the main loop and the test helpers have one.
    fn start(stdout: impl AsyncRead + 'static, stderr: impl AsyncRead + 'static) -> Self {
        let kept = Rc::new(RefCell::new(VecDeque::new()));
        let last_error = Rc::new(RefCell::new(None));
        let tasks = vec![
            tokio::task::spawn_local(copy_pipe(Box::pin(stdout), kept.clone(), None)),
            tokio::task::spawn_local(copy_pipe(
                Box::pin(stderr),
                kept.clone(),
                Some(last_error.clone()),
            )),
        ];
        Self {
            kept,
            last_error,
            tasks,
        }
    }

    /// Wait for the end of the pipes, with a time limit.
    /// Returns:
    ///   The kept output as text, and the last line of the error output
    ///   that is not empty.
    async fn finish(self) -> (String, Option<String>) {
        let ends = futures::future::join_all(self.tasks);
        let _ = tokio::time::timeout(CAPTURE_END_TIMEOUT, ends).await;
        let kept = self.kept.borrow();
        let output =
            String::from_utf8_lossy(&kept.iter().copied().collect::<Vec<_>>()).into_owned();
        (output, self.last_error.borrow().clone())
    }
}

/// Copy `pipe` to the log line by line until its end, and keep its last
/// bytes in `kept`.
/// Args:
///  - `pipe`: The output pipe
///  - `kept`: The kept output of all pipes
///  - `last_line`: Gets the last line that is not empty, if set
async fn copy_pipe(
    mut pipe: std::pin::Pin<Box<dyn AsyncRead>>,
    kept: Rc<RefCell<VecDeque<u8>>>,
    last_line: Option<Rc<RefCell<Option<String>>>>,
) {
    let mut buf = vec![0u8; 8192];
    // The part of a line that has no newline yet. It stays below the
    // capture size: a longer part goes to the log as one line.
    let mut line = Vec::new();
    let end_line = |line: &mut Vec<u8>| {
        let text = String::from_utf8_lossy(line).trim_end().to_string();
        line.clear();
        tracing::info!(target: "sandbox_process", "{text}");
        if let Some(last) = &last_line
            && !text.trim().is_empty()
        {
            *last.borrow_mut() = Some(text);
        }
    };
    while let Ok(n) = pipe.read(&mut buf).await {
        if n == 0 {
            break;
        }
        {
            let mut kept = kept.borrow_mut();
            kept.extend(&buf[..n]);
            let extra = kept.len().saturating_sub(CAPTURE_CAP);
            kept.drain(..extra);
        }
        for &byte in &buf[..n] {
            if byte == b'\n' || line.len() >= CAPTURE_CAP {
                end_line(&mut line);
            }
            if byte != b'\n' {
                line.push(byte);
            }
        }
    }
    if !line.is_empty() {
        end_line(&mut line);
    }
}

/// The signals of this process during the init of the sandbox process.
struct InitSignals {
    sigterm: Signal,
    sigint: Signal,
    /// A SIGTERM came during the init.
    got_sigterm: bool,
    /// The sandbox process detached: it left the terminal.
    detached: bool,
    /// A SIGINT came after the detach. It was for this process only.
    late_sigint: bool,
}

/// A signal of [`InitSignals`].
#[derive(Clone, Copy)]
enum InitSignal {
    Terminate,
    Interrupt,
}

impl InitSignals {
    /// Start to get SIGTERM and SIGINT. The process handles both signals
    /// from its start (see [`cli::initialize`]). Thus this does not change
    /// the effect of a signal after the init.
    fn new() -> std::io::Result<Self> {
        Ok(Self {
            sigterm: signal(SignalKind::terminate())?,
            sigint: signal(SignalKind::interrupt())?,
            got_sigterm: false,
            detached: false,
            late_sigint: false,
        })
    }

    /// Wait for the next signal.
    async fn next(&mut self) -> InitSignal {
        tokio::select! {
            Some(()) = self.sigterm.recv() => InitSignal::Terminate,
            Some(()) = self.sigint.recv() => InitSignal::Interrupt,
            else => std::future::pending().await,
        }
    }

    /// Handle `signal` for the sandbox process `pid`. A SIGTERM goes on to
    /// it. A SIGINT before the detach also goes on to it: the sandbox
    /// process decides. A SIGINT after the detach stays with this process.
    // A Ctrl+C on the terminal reaches both processes. The second SIGINT
    // has no more effect in the sandbox process. A SIGINT that only this
    // process gets, for example from `kill`, must also reach it.
    fn handle(&mut self, signal: InitSignal, pid: u32) {
        match signal {
            InitSignal::Terminate => {
                self.got_sigterm = true;
                send_signal(pid, libc::SIGTERM);
            }
            InitSignal::Interrupt if self.detached => self.late_sigint = true,
            InitSignal::Interrupt => send_signal(pid, libc::SIGINT),
        }
    }

    /// Record that the sandbox process detached.
    fn set_detached(&mut self) {
        self.detached = true;
    }

    /// End the init after the detach. A Ctrl+C during the init was for the
    /// sandbox process, and the sandbox process went on. Thus remove its
    /// mark from the interrupt flag of this process. A SIGTERM and a SIGINT
    /// after the detach stay. The flag was not set before the init (see
    /// [`spawn_program`]).
    // A SIGTERM can come just before the clear, and its stream here can
    // have it pending. The signal task of the process gets each signal at
    // the same time as this stream. Thus a SIGTERM that this stream did not
    // see is pending here after the clear, or the signal task sets the
    // flag again later.
    fn finish(mut self) {
        if !self.got_sigterm && !self.late_sigint {
            cli::clear_interrupt();
            while let Some(Some(())) = self.sigterm.recv().now_or_never() {
                self.got_sigterm = true;
            }
            if self.got_sigterm {
                cli::interrupt();
            }
        }
    }
}

/// Wait for the ready message on `reader`. The signals go to `signals`.
/// Returns:
///   The ready message, or `None` at the end of the stream.
async fn wait_ready(
    reader: &mut BufReader<OwnedReadHalf>,
    pid: u32,
    signals: &mut InitSignals,
) -> Option<Ready> {
    loop {
        tokio::select! {
            message = read_message::<ToParent>(reader, READY_CAP) => match message {
                Ok(Some(ToParent::Ready(ready))) => return Some(ready),
                // A detach comes only after the ready message.
                Ok(Some(ToParent::Detached) | None) => return None,
                Err(e) => {
                    tracing::info!("read the ready message: {e:#}");
                    return None;
                }
            },
            signal = signals.next() => signals.handle(signal, pid),
        }
    }
}

/// Wait for the exit of `child`. The signals go to `signals`.
/// Returns:
///   The exit status, or `None` if the wait fails.
async fn wait_exit(
    child: &mut tokio::process::Child,
    pid: u32,
    signals: &mut InitSignals,
) -> Option<ExitStatus> {
    loop {
        tokio::select! {
            status = child.wait() => return status.ok(),
            signal = signals.next() => signals.handle(signal, pid),
        }
    }
}

/// Get the exit code of `status`: the code of the process, 128 + the
/// signal number for a stop by a signal, or 1 if the status is unknown.
fn exit_code(status: impl Into<Option<ExitStatus>>) -> i32 {
    use std::os::unix::process::ExitStatusExt;
    let Some(status) = status.into() else {
        return 1;
    };
    status
        .code()
        .or_else(|| status.signal().map(|signal| 128 + signal))
        .unwrap_or(1)
}

/// Send the signal `signum` to the process `pid`.
fn send_signal(pid: u32, signum: libc::c_int) {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return;
    };
    if pid <= 1 {
        return;
    }
    // SAFETY: kill only sends a signal.
    unsafe {
        libc::kill(pid, signum);
    }
}
