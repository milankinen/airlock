//! Child process management inside the guest VM.
//!
//! Handles both PTY-based (interactive) and pipe-based (non-interactive)
//! processes. Each spawned process is later attached to a [`HostProcess`]
//! which bridges its I/O back to the host CLI via RPC.

use std::cell::RefCell;
use std::fmt::Display;
use std::rc::Rc;

use anyhow::Context as _;
use bytes::Bytes;
use ezpez_protocol::supervisor_capnp::*;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{error, trace};

use crate::rpc::HostProcess;

/// A child process that has been spawned but not yet wired to host I/O.
pub struct SpawnedProcess {
    child: tokio::process::Child,
    pty: Option<pty_process::Pty>,
}

impl SpawnedProcess {
    /// Wire this process's I/O to the host and block until it exits.
    /// Returns the process exit code.
    pub async fn attach(self, host: HostProcess) -> i32 {
        match self.pty {
            Some(pty) => attach_pty(self.child, pty, host).await,
            None => attach_pipe(self.child, host).await,
        }
    }
}

/// Spawn a host-side child process with inherited environment.
pub fn spawn_root(
    cmd: &str,
    args: &[&str],
    pty_size: Option<(u16, u16)>,
) -> Result<SpawnedProcess, anyhow::Error> {
    spawn(cmd, args, None, || Ok(()), pty_size)
}

/// Spawn a process inside the container rootfs via chroot + setuid/setgid.
///
/// Builds a pre-exec hook (chroot → chdir → setgid → setuid, optionally with
/// namespace/privilege hardening) and delegates to [`spawn`].
///
/// Uses a diagnostic pipe to capture which syscall failed inside the pre_exec,
/// since error strings cannot cross the fork/exec boundary — only the errno does.
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
    let cwd = cwd.to_string();
    // harden is only used in the Linux-specific pre_exec block below
    #[cfg(not(target_os = "linux"))]
    let _ = harden;

    let env_pairs: Vec<(String, String)> = env
        .iter()
        .filter_map(|e| {
            let mut parts = e.splitn(2, '=');
            let k = parts.next()?.to_string();
            let v = parts.next().unwrap_or("").to_string();
            Some((k, v))
        })
        .collect();

    // Diagnostic pipe: the pre_exec writes a static step name on failure so
    // the parent can add context to the error.  O_CLOEXEC closes it
    // automatically on exec (the success path), giving the parent clean EOF.
    // pipe2 is Linux-specific; on other platforms the diagnostic is skipped.
    #[cfg(target_os = "linux")]
    let [diag_r, diag_w] = {
        let mut fds = [-1i32; 2];
        unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) };
        fds
    };
    #[cfg(not(target_os = "linux"))]
    let [diag_r, diag_w] = [-1i32, -1i32];

    let pre_exec = move || {
        // Save errno first, then write the step tag (write(2) might change it).
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

        #[cfg(target_os = "linux")]
        if harden {
            // Prevent privilege escalation via setuid/setcap binaries.
            if unsafe { libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) } != 0 {
                fail!(b"prctl(PR_SET_NO_NEW_PRIVS)");
            }
            // Best-effort namespace isolation: private mount, IPC, and UTS
            // namespaces. Network namespace is intentionally shared so the
            // container has network access. Failures are ignored — the primary
            // security (NO_NEW_PRIVS + chroot + setuid) remains in effect.
            // Each flag is tried individually; the diagnostic pipe only fires
            // for hard failures below (chroot, setgid, setuid).
            unsafe { libc::unshare(libc::CLONE_NEWNS) };
            unsafe { libc::unshare(libc::CLONE_NEWIPC) };
            unsafe { libc::unshare(libc::CLONE_NEWUTS) };
        }

        // chroot into the assembled container rootfs
        let rootfs = std::ffi::CString::new("/mnt/overlay/rootfs").unwrap();
        if unsafe { libc::chroot(rootfs.as_ptr()) } != 0 {
            fail!(b"chroot(/mnt/overlay/rootfs)");
        }
        // chdir to the container working directory (fall back to / if missing)
        let cwd_cstr = std::ffi::CString::new(cwd.as_str()).unwrap();
        if unsafe { libc::chdir(cwd_cstr.as_ptr()) } != 0 {
            let root = std::ffi::CString::new("/").unwrap();
            unsafe { libc::chdir(root.as_ptr()) };
        }
        // setgid must come before setuid (can't change gid after dropping root)
        if unsafe { libc::setgid(gid) } != 0 {
            fail!(b"setgid");
        }
        if unsafe { libc::setuid(uid) } != 0 {
            fail!(b"setuid");
        }
        Ok(())
    };

    let result = spawn(cmd, args, Some(env_pairs), pre_exec, pty_size);

    // Close parent's write end so read() sees EOF once the child exits.
    if diag_w >= 0 {
        unsafe { libc::close(diag_w) };
    }

    // On failure, read the step tag the child wrote and add it as context.
    let result = if result.is_err() && diag_r >= 0 {
        let mut buf = [0u8; 64];
        let n = unsafe { libc::read(diag_r, buf.as_mut_ptr().cast(), buf.len()) };
        if n > 0 {
            let step = String::from_utf8_lossy(&buf[..n as usize]);
            result.with_context(|| format!("{step}"))
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

/// Core spawn primitive. Handles PTY/pipe dispatch, env setup, and an optional
/// pre-exec hook. All callers go through here.
///
/// - `env_override`: `None` → inherit environment (PTY mode also sets TERM=linux);
///   `Some(pairs)` → clear environment and replace with `pairs`.
/// - `pre_exec`: runs in the child after fork, before exec. Must only use
///   async-signal-safe operations.
fn spawn<A, F>(
    cmd: &str,
    args: &[A],
    env_override: Option<Vec<(String, String)>>,
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
        // pty_process::Command is a consuming builder — chain all calls.
        let builder = pty_process::Command::new(cmd).args(args);
        let builder = match env_override {
            Some(pairs) => builder.env_clear().envs(pairs),
            None => builder.env("TERM", "linux"),
        };
        // Safety: pre_exec runs post-fork in child; only async-signal-safe calls.
        let child = unsafe { builder.pre_exec(pre_exec) }.spawn(pts)?;
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
            .stderr(std::process::Stdio::piped());
        if let Some(pairs) = env_override {
            command.env_clear().envs(pairs);
        }
        // Safety: pre_exec runs post-fork in child; only async-signal-safe calls.
        unsafe { command.pre_exec(pre_exec) };
        let child = command.spawn()?;
        Ok(SpawnedProcess { child, pty: None })
    }
}

/// Relay I/O between a PTY-backed child and the host RPC connection.
///
/// Signals are translated: SIGINT/SIGQUIT are written as control characters
/// to the PTY (as a real terminal would), while other signals are forwarded
/// via `kill(2)`.
async fn attach_pty(
    mut child: tokio::process::Child,
    pty: pty_process::Pty,
    mut host: HostProcess,
) -> i32 {
    use std::os::unix::io::AsRawFd;
    let pty_fd = pty.as_raw_fd();
    let (mut pty_reader, pty_writer) = pty.into_split();

    // Initial size is set before spawn. This handles size changes during attach.

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
    exit_code
}

/// Relay I/O between a pipe-backed child and the host RPC connection.
/// Stdout and stderr are forwarded as separate frame types.
async fn attach_pipe(mut child: tokio::process::Child, mut host: HostProcess) -> i32 {
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
                Some(Signal::Num(signum)) => {
                    if let Some(pid) = child.id() {
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
    exit_code
}

async fn wait_child(child: &mut tokio::process::Child) -> i32 {
    match child.wait().await {
        Ok(exit) => exit.code().unwrap_or(-1),
        Err(e) => {
            error!("{e}");
            -1
        }
    }
}

fn log_error<Ok, Err: Display>(res: Result<Ok, Err>) {
    if let Err(e) = res {
        error!("{e}");
    }
}

/// Read host stdin frames and write them to the PTY, handling resize events.
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
                    tracing::trace!(
                        "guest stdin pty: {} bytes: {:?}",
                        data.len(),
                        String::from_utf8_lossy(data)
                    );
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
/// The host polls for output frames and can send signals or kill the process.
struct ProcessImpl {
    frames: RefCell<tokio::sync::mpsc::Receiver<Frame>>,
    signals: RefCell<tokio::sync::mpsc::Sender<Signal>>,
}

/// An output frame from a child process, sent to the host via polling.
enum Frame {
    Stdout(Bytes),
    Stderr(Bytes),
    Exit(i32),
}

/// A signal request from the host to the child process.
enum Signal {
    Num(i32),
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
