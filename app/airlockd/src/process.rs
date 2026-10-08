//! Processes in the guest VM.
//!
//! Starts the processes of the sandbox:
//!  * user processes in the container, with a terminal (interactive) or
//!    without. Their input and output go to the host CLI.
//!  * sidecar daemons, with their output in log files
//!
//! Also removes orphan processes after they exit.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fmt::Display;
use std::rc::Rc;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use airlock_common::supervisor_capnp::*;
use anyhow::Context as _;
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, trace};

use crate::rpc::HostProcess;

/// PIDs of processes that airlockd started with tokio and reaps itself. The
/// orphan reaper skips these PIDs, so it never takes an exit status that a
/// `Child::wait` waits for.
fn own_children() -> &'static Mutex<HashSet<i32>> {
    static R: OnceLock<Mutex<HashSet<i32>>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(HashSet::new()))
}

/// Record a PID that airlockd started itself, so the orphan reaper skips it.
pub(crate) fn register_own_child(pid: u32) {
    own_children().lock().unwrap().insert(pid as i32);
}

/// Reap orphaned zombie processes that the kernel gives to this init
/// process. Runs forever, with a check every 2 seconds.
///
/// airlockd runs as PID 1. Thus each process whose parent exits (for
/// example a double-forking daemon of the workload) gets airlockd as its
/// new parent. tokio reaps only the children that it started. Without this
/// reaper, the orphans stay as `<defunct>` zombies until no PIDs are left
/// and the VM cannot fork.
pub async fn run_orphan_reaper() {
    // Scan `/proc` for zombie children of this process and reap them. Skip
    // the PIDs that airlockd started itself (see `own_children`), so tokio's
    // `Child::wait` still sees their exits. The scan uses `/proc`, not
    // `waitpid(-1)`, because `waitpid(-1)` takes the first zombie, also a
    // child that airlockd started itself.
    let me = std::process::id() as i32;
    let mut ticker = tokio::time::interval(Duration::from_secs(2));
    loop {
        ticker.tick().await;
        reap_orphans_once(me);
    }
}

/// Do one scan of `/proc` and reap the orphan zombie children of `me`.
fn reap_orphans_once(me: i32) {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return;
    };
    let mut live: HashSet<i32> = HashSet::new();
    for entry in entries.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|s| s.parse::<i32>().ok())
        else {
            continue;
        };
        live.insert(pid);

        let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
            continue;
        };
        let Some((ppid, state)) = parse_stat(&stat) else {
            continue;
        };
        if ppid == me && state == 'Z' && !own_children().lock().unwrap().contains(&pid) {
            // An orphan zombie that airlockd did not start. Reap it.
            unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) };
        }
    }
    // Remove the entries of processes that do not exist now (tokio reaped
    // them). This keeps the set small, and a reused PID can register again
    // cleanly at spawn.
    own_children()
        .lock()
        .unwrap()
        .retain(|pid| live.contains(pid));
}

/// Parse the parent PID and state character from a `/proc/<pid>/stat` line.
///
/// The `comm` field (field 2) is in parentheses, and it can contain spaces
/// and parentheses itself. Thus the parser splits *after* the last `)`. The
/// state is the first field after it, and the PPID is the second.
fn parse_stat(stat: &str) -> Option<(i32, char)> {
    let after_comm = stat[stat.rfind(')')? + 1..].trim_start();
    let mut fields = after_comm.split_whitespace();
    let state = fields.next()?.chars().next()?;
    let ppid = fields.next()?.parse::<i32>().ok()?;
    Some((ppid, state))
}

/// A child process that runs, but has no connection to the host I/O yet.
pub struct SpawnedProcess {
    child: tokio::process::Child,
    pty: Option<pty_process::Pty>,
}

impl SpawnedProcess {
    /// Connect the process I/O to the host and wait until the process exits.
    /// The host gets the exit code as the last frame.
    pub async fn attach(self, host: HostProcess) {
        match self.pty {
            Some(pty) => attach_pty(self.child, pty, host).await,
            None => attach_pipe(self.child, host).await,
        }
    }
}

/// Start a user process inside the container rootfs, as the container user.
/// Args:
///  - `cmd`: Program to run
///  - `args`: Program arguments
///  - `env`: Full environment as `KEY=VALUE` strings. Nothing is inherited.
///  - `cwd`: Working directory in the container. If it does not exist, the
///    process starts in `/`.
///  - `uid`, `gid`: Container user and group
///  - `harden`: Apply `PR_SET_NO_NEW_PRIVS` and private mount, IPC and UTS
///    namespaces
///  - `pty_size`: `(rows, cols)` to run the process in a PTY. `None` runs it
///    with pipes.
///
/// Returns:
///   The started process, or an error that tells which setup step failed.
#[allow(clippy::too_many_arguments)]
pub fn spawn_user(
    cmd: &str,
    args: &[String],
    env: &[String],
    cwd: &str,
    uid: u32,
    gid: u32,
    harden: bool,
    pty_size: Option<(u16, u16)>,
) -> Result<SpawnedProcess, anyhow::Error> {
    let env_pairs = env_string_pairs(env);
    // The diagnostic pipe tells which syscall failed in the pre-exec hook.
    // Error strings cannot cross the fork/exec boundary. Only the errno can.
    let (diag_r, diag_w) = open_diag_pipe();
    let pre_exec = build_pre_exec(cwd.to_string(), uid, gid, harden, diag_w);

    let result = spawn(cmd, args, env_pairs, pre_exec, pty_size);
    finish_diag_pipe(diag_r, diag_w, result)
}

/// Start a sidecar daemon process, with stdout and stderr in files.
///
/// Same container setup as [`spawn_user`], but stdin is `/dev/null` (there
/// is no host stream).
/// Args:
///  - `cmd`, `args`, `env`, `cwd`, `uid`, `gid`, `harden`: Same as in
///    [`spawn_user`]
///  - `stdout_file`, `stderr_file`: Log files. The child gets duplicates of
///    these FDs, so each restart writes after the output of the previous run
///    (the file offset is shared).
///
/// Returns:
///   The child process. There is no [`HostProcess`] to attach, and the
///   restart loop of the daemon owns the child.
#[allow(clippy::too_many_arguments)]
pub fn spawn_daemon(
    cmd: &str,
    args: &[String],
    env: &[String],
    cwd: &str,
    uid: u32,
    gid: u32,
    harden: bool,
    stdout_file: &std::fs::File,
    stderr_file: &std::fs::File,
) -> anyhow::Result<tokio::process::Child> {
    use std::process::Stdio;

    let env_pairs = env_string_pairs(env);
    let (diag_r, diag_w) = open_diag_pipe();
    let pre_exec = build_pre_exec(cwd.to_string(), uid, gid, harden, diag_w);

    let stdout_dup = stdout_file.try_clone().context("dup daemon stdout")?;
    let stderr_dup = stderr_file.try_clone().context("dup daemon stderr")?;

    let mut command = tokio::process::Command::new(cmd);
    command
        .args(args)
        .env_clear()
        .envs(env_pairs)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_dup))
        .stderr(Stdio::from(stderr_dup));
    // Safety: pre_exec runs in the child after fork. It uses only
    // async-signal-safe calls.
    unsafe { command.pre_exec(pre_exec) };
    let result = command.spawn().map_err(anyhow::Error::from);
    let result = finish_diag_pipe(diag_r, diag_w, result);
    if let Ok(child) = &result
        && let Some(pid) = child.id()
    {
        register_own_child(pid);
    }
    result
}

/// Split `KEY=VALUE` strings into pairs. A string without `=` gets an empty
/// value.
fn env_string_pairs(env: &[String]) -> Vec<(String, String)> {
    env.iter()
        .filter_map(|e| {
            let mut parts = e.splitn(2, '=');
            let k = parts.next()?.to_string();
            let v = parts.next().unwrap_or("").to_string();
            Some((k, v))
        })
        .collect()
}

/// Open the diagnostic pipe. The pre-exec hook uses it to report the step
/// that failed. Returns `(read_fd, write_fd)`, or `(-1, -1)` on other
/// targets than Linux.
///
/// `O_CLOEXEC` closes the write end when exec succeeds, so the parent gets
/// a clean EOF.
fn open_diag_pipe() -> (i32, i32) {
    #[cfg(target_os = "linux")]
    {
        let mut fds = [-1i32; 2];
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        (fds[0], fds[1])
    }
    #[cfg(not(target_os = "linux"))]
    {
        (-1, -1)
    }
}

/// Close the pipe FDs. If the spawn failed, read the step tag that the
/// pre-exec hook wrote and add it to the error as context.
fn finish_diag_pipe<T>(diag_r: i32, diag_w: i32, result: anyhow::Result<T>) -> anyhow::Result<T> {
    if diag_w >= 0 {
        unsafe { libc::close(diag_w) };
    }
    let result = if result.is_err() && diag_r >= 0 {
        let mut buf = [0u8; 64];
        let n = unsafe { libc::read(diag_r, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            let step = String::from_utf8_lossy(&buf[..n as usize]).into_owned();
            result.with_context(|| step)
        } else {
            result
        }
    } else {
        result
    };
    if diag_r >= 0 {
        unsafe { libc::close(diag_r) };
    }
    result
}

/// Make the pre-exec hook that runs in the child after fork.
///
/// Steps: enter the container rootfs (join the sandbox mount namespace, or
/// chroot if init could not make it), harden, chdir, setgroups, setgid,
/// setuid. On a hard failure, the hook writes a short step tag to `diag_w`,
/// so the parent can add it to the error as context.
fn build_pre_exec(
    cwd: String,
    uid: u32,
    gid: u32,
    harden: bool,
    diag_w: i32,
) -> impl FnMut() -> std::io::Result<()> + Send + Sync + 'static {
    // Allocate before fork. The hook can only make raw syscalls.
    let rootfs = std::ffi::CString::new(crate::sandbox_ns::ROOTFS).unwrap();
    let ns_fd = crate::sandbox_ns::fd();
    // Only Linux uses `harden` and `ns_fd`. They are always there, so the
    // code is the same on all platforms.
    #[cfg(not(target_os = "linux"))]
    let _ = (harden, ns_fd);

    move || {
        // Save errno first, then write the step tag (write(2) can change it).
        macro_rules! fail {
            ($tag:expr) => {{
                let err = std::io::Error::last_os_error();
                if diag_w >= 0 {
                    let b: &[u8] = $tag;
                    unsafe { libc::write(diag_w, b.as_ptr().cast(), b.len()) };
                }
                return Err(err);
            }};
        }

        // Enter the container rootfs. Join the shared sandbox mount namespace
        // (its root is the rootfs, see `crate::sandbox_ns`), or use chroot if
        // init could not make it. setns must come before the hardening
        // unshare below. The unshare then makes a private copy of this
        // namespace. setns also moves the cwd to the new root. The chdir
        // below sets the correct cwd.
        #[cfg(target_os = "linux")]
        let entered = match ns_fd {
            Some(fd) => {
                if unsafe { libc::setns(fd, libc::CLONE_NEWNS) } != 0 {
                    fail!(b"setns(sandbox mount ns)");
                }
                true
            }
            None => false,
        };
        #[cfg(not(target_os = "linux"))]
        let entered = false;

        #[cfg(target_os = "linux")]
        if harden {
            // Prevent privilege escalation through setuid/setcap binaries.
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
                fail!(b"prctl(PR_SET_NO_NEW_PRIVS)");
            }
            // Best-effort namespace isolation: private mount, IPC and UTS
            // namespaces. The network namespace is shared on purpose, so the
            // container has network access. Failures are ignored, because
            // the primary security (NO_NEW_PRIVS, the rootfs root, setuid)
            // stays.
            unsafe { libc::unshare(libc::CLONE_NEWNS) };
            unsafe { libc::unshare(libc::CLONE_NEWIPC) };
            unsafe { libc::unshare(libc::CLONE_NEWUTS) };
        }

        if !entered && unsafe { libc::chroot(rootfs.as_ptr()) } != 0 {
            fail!(b"chroot(/mnt/overlay/rootfs)");
        }
        let cwd_cstr = std::ffi::CString::new(cwd.as_str()).unwrap();
        if unsafe { libc::chdir(cwd_cstr.as_ptr()) } != 0 {
            let root = std::ffi::CString::new("/").unwrap();
            unsafe { libc::chdir(root.as_ptr()) };
        }
        // Remove the supplementary groups before the privilege drop.
        // Otherwise the process keeps the groups of airlockd (root), which
        // usually include GID 0, also with an unprivileged uid. Must run as
        // root, before setuid. `setgroups` is async-signal-safe (a bare
        // syscall).
        if unsafe { libc::setgroups(0, std::ptr::null()) } != 0 {
            fail!(b"setgroups");
        }
        // setgid must run before setuid. After setuid drops root, the
        // process cannot change its gid.
        if unsafe { libc::setgid(gid) } != 0 {
            fail!(b"setgid");
        }
        if unsafe { libc::setuid(uid) } != 0 {
            fail!(b"setuid");
        }
        Ok(())
    }
}

/// Start a child process with a PTY or with pipes. All spawn functions
/// except [`spawn_daemon`] use it.
/// Args:
///  - `cmd`, `args`: Program and its arguments
///  - `env`: Full environment of the child. Nothing is inherited.
///  - `pre_exec`: Hook that runs in the child after fork, before exec. Must
///    use only async-signal-safe operations.
///  - `pty_size`: `(rows, cols)` for a PTY. `None` uses pipes.
fn spawn<A, F>(
    cmd: &str,
    args: &[A],
    env: Vec<(String, String)>,
    pre_exec: F,
    pty_size: Option<(u16, u16)>,
) -> Result<SpawnedProcess, anyhow::Error>
where
    A: AsRef<std::ffi::OsStr>,
    F: FnMut() -> std::io::Result<()> + Send + Sync + 'static,
{
    if let Some((rows, cols)) = pty_size {
        let (pty, pts) = pty_process::open()?;
        tracing::debug!("pty initial size: {rows}x{cols}");
        if let Err(e) = pty.resize(pty_process::Size::new(rows, cols)) {
            tracing::warn!("initial pty resize failed: {e}");
        }
        // pty_process::Command is a consuming builder. Chain all calls.
        let builder = pty_process::Command::new(cmd)
            .args(args)
            .env_clear()
            .envs(env);
        // Safety: pre_exec runs in the child after fork. It uses only
        // async-signal-safe calls.
        let child = unsafe { builder.pre_exec(pre_exec) }.spawn(pts)?;
        if let Some(pid) = child.id() {
            register_own_child(pid);
        }
        Ok(SpawnedProcess {
            child,
            pty: Some(pty),
        })
    } else {
        let mut command = tokio::process::Command::new(cmd);
        command
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // Its own process group, as a PTY child has its own session.
            // Thus host signals also reach the children of a shell (see
            // `signal_group`).
            .process_group(0);
        command.env_clear().envs(env);
        // Safety: pre_exec runs in the child after fork. It uses only
        // async-signal-safe calls.
        unsafe { command.pre_exec(pre_exec) };
        let child = command.spawn()?;
        if let Some(pid) = child.id() {
            register_own_child(pid);
        }
        Ok(SpawnedProcess { child, pty: None })
    }
}

/// Relay I/O between a PTY-backed child and the host RPC connection.
///
/// SIGINT and SIGQUIT go to the PTY as control characters, as from a real
/// terminal. Other signals go to the child with `kill(2)`.
async fn attach_pty(
    mut child: tokio::process::Child,
    pty: pty_process::Pty,
    mut host: HostProcess,
) {
    use std::os::unix::io::AsRawFd;
    let pty_fd = pty.as_raw_fd();
    let (mut pty_reader, pty_writer) = pty.into_split();

    // The initial size is set before spawn. `relay_stdin_pty` handles the
    // size changes during attach.

    let stdin = host.stdin;
    tokio::task::spawn_local(async move {
        if let Err(e) = relay_stdin_pty(stdin, pty_writer).await {
            error!("stdin failure: {e:#}");
        }
    });

    let (signals_tx, mut signals_rx) = tokio::sync::mpsc::channel(1);
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<Frame>(1);
    if let Some(tx) = host.result.take() {
        let _ = tx.send(Ok(capnp_rpc::new_client(ProcessImpl {
            frames: RefCell::new(frames_rx),
            signals: RefCell::new(signals_tx),
        })));
    }

    let mut buf = [0u8; 4096];
    loop {
        tokio::select! {
            res = pty_reader.read(&mut buf) => match res {
                Ok(0) | Err(_) => break,
                Ok(n) => log_error(frames_tx.send(Frame::Stdout(Bytes::copy_from_slice(&buf[..n]))).await),
            },
            s = signals_rx.recv() => match s {
                Some(Signal::Num(signum)) => {
                    trace!("signal ({signum}), pid: {:?}", child.id());
                    let ctrl = match signum {
                        2 => Some(0x03u8),
                        3 => Some(0x1cu8),
                        _ => None,
                    };
                    if let Some(ch) = ctrl {
                        unsafe { libc::write(pty_fd, (&raw const ch).cast(), 1) };
                    } else if let Some(pid) = child.id() {
                        unsafe { libc::kill(pid as i32, signum) };
                    }
                },
                Some(Signal::Kill) => {
                    log_error(child.start_kill());
                    break;
                },
                None => break,
            }
        }
    }

    let exit_code = wait_child(&mut child).await;
    log_error(frames_tx.send(Frame::Exit(exit_code)).await);
}

/// Relay I/O between a pipe-backed child and the host RPC connection.
/// Stdout and stderr go to the host as different frame types.
async fn attach_pipe(mut child: tokio::process::Child, mut host: HostProcess) {
    let child_stdin = child.stdin.take();
    let mut child_stdout = child.stdout.take();
    let mut child_stderr = child.stderr.take();

    let stdin = host.stdin;
    tokio::task::spawn_local(async move {
        if let Some(mut w) = child_stdin
            && let Err(e) = relay_stdin_pipe(stdin, &mut w).await
        {
            error!("stdin failure: {e:#}");
        }
    });

    let (signals_tx, mut signals_rx) = tokio::sync::mpsc::channel(1);
    let (frames_tx, frames_rx) = tokio::sync::mpsc::channel::<Frame>(1);
    if let Some(tx) = host.result.take() {
        let _ = tx.send(Ok(capnp_rpc::new_client(ProcessImpl {
            frames: RefCell::new(frames_rx),
            signals: RefCell::new(signals_tx),
        })));
    }

    let mut stdout_buf = [0u8; 4096];
    let mut stderr_buf = [0u8; 4096];
    let mut stdout_done = false;
    let mut stderr_done = false;

    loop {
        if stdout_done && stderr_done {
            break;
        }
        tokio::select! {
            res = async { child_stdout.as_mut().unwrap().read(&mut stdout_buf).await },
                if !stdout_done && child_stdout.is_some() =>
            {
                match res {
                    Ok(0) | Err(_) => stdout_done = true,
                    Ok(n) => log_error(frames_tx.send(Frame::Stdout(Bytes::copy_from_slice(&stdout_buf[..n]))).await),
                }
            },
            res = async { child_stderr.as_mut().unwrap().read(&mut stderr_buf).await },
                if !stderr_done && child_stderr.is_some() =>
            {
                match res {
                    Ok(0) | Err(_) => stderr_done = true,
                    Ok(n) => log_error(frames_tx.send(Frame::Stderr(Bytes::copy_from_slice(&stderr_buf[..n]))).await),
                }
            },
            s = signals_rx.recv() => match s {
                Some(Signal::Num(signum)) => signal_group(&child, signum),
                Some(Signal::Kill) => {
                    signal_group(&child, libc::SIGKILL);
                    log_error(child.start_kill());
                    break;
                },
                None => break,
            }
        }
    }

    let exit_code = wait_child(&mut child).await;
    log_error(frames_tx.send(Frame::Exit(exit_code)).await);
}

/// Send `signum` to the process group of a pipe-mode child (the child leads
/// its own group, see [`spawn`]). Thus the processes that it started also
/// get the signal. The group exists until its last member exits, also after
/// the leader exits.
fn signal_group(child: &tokio::process::Child, signum: i32) {
    if let Some(pid) = child.id() {
        trace!("signal ({signum}), process group: {pid}");
        unsafe { libc::kill(-(pid as i32), signum) };
    }
}

/// Wait for the child to exit and return its exit code. Returns -1 if the
/// child has no exit code (for example, a signal stopped it) or on error.
async fn wait_child(child: &mut tokio::process::Child) -> i32 {
    match child.wait().await {
        Ok(exit) => exit.code().unwrap_or(-1),
        Err(e) => {
            error!("{e}");
            -1
        }
    }
}

/// Log the error of `res`, if any.
fn log_error<Ok, Err: Display>(res: Result<Ok, Err>) {
    if let Err(e) = res {
        error!("{e}");
    }
}

/// Read host stdin frames and write them to the PTY. Also applies the
/// resize events.
async fn relay_stdin_pty(
    stdin: stdin::Client,
    mut writer: pty_process::OwnedWritePty,
) -> anyhow::Result<()> {
    loop {
        let response = stdin.read_request().send().promise.await?;
        let input = response.get()?.get_input()?;
        match input.which()? {
            process_input::Stdin(frame) => {
                if let Ok(data_frame::Data(Ok(data))) = frame?.which() {
                    // Log only the byte count. stdin can contain secrets (for
                    // example a pasted token).
                    tracing::trace!("guest stdin pty: {} bytes", data.len());
                    writer.write_all(data).await?;
                } else {
                    tracing::trace!("guest stdin pty: EOF");
                    return Ok(());
                }
            }
            process_input::Resize(size) => {
                let s = size?;
                tracing::debug!("pty resize: {}x{}", s.get_rows(), s.get_cols());
                writer.resize(pty_process::Size::new(s.get_rows(), s.get_cols()))?;
            }
        }
    }
}

/// Read host stdin frames and write them to the child's pipe stdin.
async fn relay_stdin_pipe(
    stdin: stdin::Client,
    writer: &mut tokio::process::ChildStdin,
) -> anyhow::Result<()> {
    loop {
        let response = stdin.read_request().send().promise.await?;
        let input = response.get()?.get_input()?;
        match input.which()? {
            process_input::Stdin(frame) => match frame?.which() {
                Ok(data_frame::Data(Ok(data))) => writer.write_all(data).await?,
                _ => return Ok(()),
            },
            process_input::Resize(_) => {} // ignored in pipe mode
        }
    }
}

/// Server-side implementation of the Cap'n Proto `Process` interface.
///
/// The host polls for output frames, and can send signals or kill the
/// process.
struct ProcessImpl {
    /// Output frames from the relay loop.
    frames: RefCell<tokio::sync::mpsc::Receiver<Frame>>,
    /// Signal requests to the relay loop.
    signals: RefCell<tokio::sync::mpsc::Sender<Signal>>,
}

/// An output frame from a child process. The host gets it when it polls.
enum Frame {
    /// Data from stdout (or from the PTY).
    Stdout(Bytes),
    /// Data from stderr (pipe mode only).
    Stderr(Bytes),
    /// Exit code. The last frame.
    Exit(i32),
}

/// A signal request from the host to the child process.
enum Signal {
    /// Send this signal number.
    Num(i32),
    /// Kill the process and stop the relay.
    Kill,
}

impl process::Server for ProcessImpl {
    #[allow(clippy::await_holding_refcell_ref)]
    async fn poll(
        self: Rc<Self>,
        _params: process::PollParams,
        mut results: process::PollResults,
    ) -> Result<(), capnp::Error> {
        let mut next = results.get().init_next();
        match self.frames.borrow_mut().recv().await {
            Some(Frame::Stdout(data)) => next.init_stdout().set_data(&data),
            Some(Frame::Stderr(data)) => next.init_stderr().set_data(&data),
            Some(Frame::Exit(code)) => next.set_exit(code),
            None => {
                return Err(capnp::Error::failed(
                    "supervisor process already exited".into(),
                ));
            }
        }
        Ok(())
    }

    async fn signal(
        self: Rc<Self>,
        params: process::SignalParams,
        _results: process::SignalResults,
    ) -> Result<(), capnp::Error> {
        let signum = params.get()?.get_signum();
        let tx = self.signals.borrow().clone();
        let _ = tx.send(Signal::Num(signum)).await;
        Ok(())
    }

    async fn kill(
        self: Rc<Self>,
        _params: process::KillParams,
        _results: process::KillResults,
    ) -> Result<(), capnp::Error> {
        let tx = self.signals.borrow().clone();
        let _ = tx.send(Signal::Kill).await;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Tests of the `/proc/<pid>/stat` parser.

    use super::parse_stat;

    /// Test that the parser reads the PPID and state after the last `)` of
    /// the command name. A command name can contain spaces and parentheses.
    ///   1. Parse a normal line and a line with `)` in the command name
    ///   2. Check the PPID and state, and check that a bad line gives `None`
    #[test]
    fn parse_stat_reads_ppid_and_state_after_last_paren_of_comm() {
        assert_eq!(parse_stat("1234 (bash) S 1 1234 1234 0 -1"), Some((1, 'S')));
        assert_eq!(
            parse_stat("42 (weird ) proc) Z 7 42 42 0 -1 4194560"),
            Some((7, 'Z'))
        );
        assert_eq!(parse_stat("not a stat line"), None);
    }
}
