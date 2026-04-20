//! Sidecar daemons — long-running processes declared under `[daemons.<name>]`
//! that run in parallel with the main shell.
//!
//! Each daemon runs inside its own local task that owns the restart loop,
//! graceful shutdown, and stdout/stderr file handles. The shared
//! `states` map lets the supervisor RPC surface a snapshot (`pollDaemons`)
//! without touching the per-daemon tasks directly. A `oneshot::Sender` per
//! daemon is the shutdown signal; dropping it is equivalent to sending it
//! (the task sees the channel close).
//!
//! Log paths are pre-chroot (`/mnt/overlay/rootfs/airlock/daemons/<name>/…`)
//! because the supervisor is not itself chrooted. The inherited stdio FDs
//! survive the child's chroot — they reference the open file description,
//! not the path.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;
use std::time::Duration;

use airlock_common::supervisor_capnp::{
    DaemonState as WireDaemonState, RestartPolicy as WireRestartPolicy, daemon_spec, daemon_status,
};
use tokio::sync::oneshot;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::process::spawn_daemon;

const LOG_ROOT: &str = "/mnt/overlay/rootfs/airlock/daemons";

/// Full daemon spec the guest receives via capnp. Mirrors `DaemonSpec` in
/// the schema, translated into owned Rust types by `from_capnp`.
pub struct DaemonSpec {
    pub name: String,
    pub command: Vec<String>,
    pub env: Vec<String>,
    pub cwd: String,
    pub signal: i32,
    pub timeout_ms: u32,
    pub restart: RestartPolicy,
    pub max_restarts: u32,
    pub harden: bool,
}

impl DaemonSpec {
    /// Translate a wire-format `daemon_spec` reader into an owned spec.
    pub fn from_capnp(d: daemon_spec::Reader) -> Result<Self, capnp::Error> {
        let command = d
            .get_command()?
            .iter()
            .map(|s| Ok(s?.to_str()?.to_string()))
            .collect::<Result<Vec<_>, capnp::Error>>()?;
        let env = d
            .get_env()?
            .iter()
            .map(|s| Ok(s?.to_str()?.to_string()))
            .collect::<Result<Vec<_>, capnp::Error>>()?;
        let restart = match d.get_restart()? {
            WireRestartPolicy::Always => RestartPolicy::Always,
            WireRestartPolicy::OnFailure => RestartPolicy::OnFailure,
        };
        Ok(Self {
            name: d.get_name()?.to_str()?.to_string(),
            command,
            env,
            cwd: d.get_cwd()?.to_str()?.to_string(),
            signal: d.get_signal(),
            timeout_ms: d.get_timeout_ms(),
            restart,
            max_restarts: d.get_max_restarts(),
            harden: d.get_harden(),
        })
    }
}

/// Parse the full `daemons` list from a start-request params reader.
pub fn parse_specs(
    readers: capnp::struct_list::Reader<daemon_spec::Owned>,
) -> Result<Vec<DaemonSpec>, capnp::Error> {
    readers.iter().map(DaemonSpec::from_capnp).collect()
}

/// Serialize a snapshot into a pre-initialized `pollDaemons` response list.
/// The caller is responsible for sizing the list via `init_states(len)`.
pub fn write_status_list(
    snapshot: &[(String, DaemonState)],
    mut list: capnp::struct_list::Builder<daemon_status::Owned>,
) {
    for (i, (name, state)) in snapshot.iter().enumerate() {
        let mut entry = list.reborrow().get(i as u32);
        entry.set_name(name.as_str());
        entry.set_state(match state {
            DaemonState::Running => WireDaemonState::Running,
            DaemonState::Stopped => WireDaemonState::Stopped,
            DaemonState::Killed => WireDaemonState::Killed,
        });
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    Always,
    OnFailure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    Running,
    Stopped,
    Killed,
}

/// Collection of running daemons. Lifetime matches the sandbox run: created
/// once inside `Supervisor.start()`, dropped when the VM shuts down.
pub struct DaemonSet {
    states: Rc<RefCell<BTreeMap<String, DaemonState>>>,
    stops: RefCell<BTreeMap<String, oneshot::Sender<()>>>,
}

impl DaemonSet {
    /// Spawn one local task per daemon and return the set. Construction
    /// never blocks on the daemons themselves — each task enters its own
    /// restart loop, so first-start failure becomes a restart attempt
    /// rather than a top-level error.
    pub fn start_all(specs: Vec<DaemonSpec>, uid: u32, gid: u32) -> Self {
        let states: Rc<RefCell<BTreeMap<String, DaemonState>>> =
            Rc::new(RefCell::new(BTreeMap::new()));
        let mut stops = BTreeMap::new();

        for spec in specs {
            let (stop_tx, stop_rx) = oneshot::channel();
            states
                .borrow_mut()
                .insert(spec.name.clone(), DaemonState::Running);
            stops.insert(spec.name.clone(), stop_tx);
            let states_for_task = Rc::clone(&states);
            tokio::task::spawn_local(async move {
                run_daemon(spec, uid, gid, stop_rx, states_for_task).await;
            });
        }

        Self {
            states,
            stops: RefCell::new(stops),
        }
    }

    /// Snapshot every declared daemon's current state. Called from the
    /// `pollDaemons` RPC; order is stable because `BTreeMap` is sorted.
    pub fn snapshot(&self) -> Vec<(String, DaemonState)> {
        self.states
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    /// Ask every still-running daemon to shut down. Drops every stop
    /// channel, which the per-daemon tasks observe as a signal to start
    /// `graceful_stop`. Idempotent — a second call sends nothing.
    pub fn shutdown_all(&self) {
        let mut stops = self.stops.borrow_mut();
        for (_, tx) in std::mem::take(&mut *stops) {
            let _ = tx.send(());
        }
    }
}

async fn run_daemon(
    spec: DaemonSpec,
    uid: u32,
    gid: u32,
    mut stop_rx: oneshot::Receiver<()>,
    states: Rc<RefCell<BTreeMap<String, DaemonState>>>,
) {
    let (stdout_file, stderr_file) = match open_log_files(&spec.name) {
        Ok(pair) => pair,
        Err(e) => {
            error!("daemon {}: failed to open log files: {e:#}", spec.name);
            states
                .borrow_mut()
                .insert(spec.name.clone(), DaemonState::Stopped);
            return;
        }
    };

    let mut attempts: u32 = 0;
    loop {
        if attempts > 0 {
            let wait = Duration::from_secs(u64::from(attempts));
            info!("daemon {}: waiting {:?} before restart", spec.name, wait);
            tokio::select! {
                () = sleep(wait) => {}
                _ = &mut stop_rx => {
                    states.borrow_mut().insert(spec.name.clone(), DaemonState::Stopped);
                    return;
                }
            }
        }

        let child = spawn_daemon(
            &spec.command[0],
            &spec.command[1..],
            &spec.env,
            &spec.cwd,
            uid,
            gid,
            spec.harden,
            &stdout_file,
            &stderr_file,
        );

        let mut child = match child {
            Ok(c) => {
                info!(
                    "daemon {}: started (pid={:?}) attempt={}",
                    spec.name,
                    c.id(),
                    attempts + 1
                );
                c
            }
            Err(e) => {
                warn!("daemon {}: spawn failed: {e:#}", spec.name);
                attempts += 1;
                if spec.max_restarts > 0 && attempts > spec.max_restarts {
                    break;
                }
                continue;
            }
        };

        let exit = tokio::select! {
            code = child.wait() => Exit::Code(code.ok().and_then(|s| s.code()).unwrap_or(-1)),
            _ = &mut stop_rx => Exit::StopRequested,
        };

        match exit {
            Exit::StopRequested => {
                let killed = graceful_stop(child, spec.signal, spec.timeout_ms).await;
                let state = if killed {
                    DaemonState::Killed
                } else {
                    DaemonState::Stopped
                };
                info!("daemon {}: shutdown → {state:?}", spec.name);
                states.borrow_mut().insert(spec.name.clone(), state);
                return;
            }
            Exit::Code(code) => {
                info!("daemon {}: exited with code {code}", spec.name);
                attempts += 1;
                let should_restart = match spec.restart {
                    RestartPolicy::Always => true,
                    RestartPolicy::OnFailure => code != 0,
                };
                if !should_restart {
                    break;
                }
                if spec.max_restarts > 0 && attempts > spec.max_restarts {
                    warn!(
                        "daemon {}: reached max_restarts={}, giving up",
                        spec.name, spec.max_restarts
                    );
                    break;
                }
            }
        }
    }

    states
        .borrow_mut()
        .insert(spec.name.clone(), DaemonState::Stopped);
}

enum Exit {
    Code(i32),
    StopRequested,
}

/// Send the configured shutdown signal, wait up to `timeout_ms`, then
/// SIGKILL if the child is still alive. Returns `true` iff SIGKILL was
/// issued (i.e. the daemon ended in `Killed` rather than `Stopped`).
/// `timeout_ms == 0` means wait forever — the SIGKILL branch is skipped.
async fn graceful_stop(mut child: tokio::process::Child, signal: i32, timeout_ms: u32) -> bool {
    let Some(pid) = child.id() else {
        // Child already exited; nothing to signal.
        let _ = child.wait().await;
        return false;
    };
    unsafe { libc::kill(pid as i32, signal) };

    if timeout_ms == 0 {
        let _ = child.wait().await;
        return false;
    }

    let timeout = Duration::from_millis(u64::from(timeout_ms));
    tokio::select! {
        _ = child.wait() => false,
        () = sleep(timeout) => {
            if let Some(pid) = child.id() {
                unsafe { libc::kill(pid as i32, libc::SIGKILL) };
            }
            let _ = child.wait().await;
            true
        }
    }
}

/// Truncate-and-create the per-daemon log files. Opened once per sandbox
/// lifetime; every restart dup's the same FD so the file offset advances
/// across restarts (append-on-restart, truncate-on-sandbox-restart).
fn open_log_files(name: &str) -> anyhow::Result<(std::fs::File, std::fs::File)> {
    let dir = PathBuf::from(LOG_ROOT).join(name);
    std::fs::create_dir_all(&dir)?;
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join("stdout.log"))?;
    let stderr = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dir.join("stderr.log"))?;
    Ok((stdout, stderr))
}
