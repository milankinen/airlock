//! Sidecar daemons.
//!
//! Daemons are long-running processes from the `[daemons.<name>]` config.
//! They start during boot, before the host starts any other process. This
//! module starts, monitors and stops the daemons, and reports their state to
//! the host.

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

/// Root directory of the daemon log files (`<name>/stdout.log`,
/// `<name>/stderr.log`).
///
/// The path is in the VM mount namespace, because the supervisor stays
/// there. The inherited stdio FDs stay valid after the child enters the
/// sandbox, because they refer to the open file description, not to the
/// path.
const LOG_ROOT: &str = "/mnt/overlay/rootfs/airlock/daemons";

/// Daemon spec from the host. Owned Rust version of `DaemonSpec` in the
/// capnp schema.
pub struct DaemonSpec {
    /// Daemon name from `[daemons.<name>]`.
    pub name: String,
    /// Program and its arguments. The first item is the program.
    pub command: Vec<String>,
    /// Environment variables as `KEY=VALUE` strings.
    pub env: Vec<String>,
    /// Working directory inside the container.
    pub cwd: String,
    /// Signal that asks the daemon to stop.
    pub signal: i32,
    /// Time to wait after `signal` before SIGKILL. 0 means wait forever.
    pub timeout_ms: u32,
    /// When to restart the daemon after it exits.
    pub restart: RestartPolicy,
    /// Maximum number of restarts. 0 means no limit.
    pub max_restarts: u32,
    /// Apply `PR_SET_NO_NEW_PRIVS` and private mount, IPC and UTS namespaces
    /// (see [`crate::process::spawn_user`]).
    pub harden: bool,
}

impl DaemonSpec {
    /// Convert a wire-format `daemon_spec` reader into an owned spec.
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

/// Parse the full `daemons` list from the request params.
pub fn parse_specs(
    readers: capnp::struct_list::Reader<daemon_spec::Owned>,
) -> Result<Vec<DaemonSpec>, capnp::Error> {
    readers.iter().map(DaemonSpec::from_capnp).collect()
}

/// Write a daemon state snapshot into a `pollDaemons` response list.
/// Args:
///  - `snapshot`: Daemon names and states, from [`DaemonSet::snapshot`]
///  - `list`: Response list. The caller must make it the same size as
///    `snapshot` with `init_states(len)`.
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

/// When to restart a daemon after it exits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RestartPolicy {
    /// Restart after every exit.
    Always,
    /// Restart only after a non-zero exit code.
    OnFailure,
}

/// Current state of a daemon.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DaemonState {
    /// The daemon runs, or waits for a restart.
    Running,
    /// The daemon exited and does not restart.
    Stopped,
    /// The daemon did not stop in time after the stop signal and received
    /// SIGKILL.
    Killed,
}

/// Collection of running daemons. Exists for the full sandbox run: created
/// once on `Supervisor.boot()`, dropped when the VM stops.
pub struct DaemonSet {
    /// Shared state map. RPC calls read a snapshot (`pollDaemons`) from it
    /// without access to the per-daemon tasks.
    states: Rc<RefCell<BTreeMap<String, DaemonState>>>,
    /// Stop signal for each daemon. A dropped sender has the same effect
    /// as a sent value: the task sees the channel close.
    stops: RefCell<BTreeMap<String, oneshot::Sender<()>>>,
}

impl DaemonSet {
    /// Start all daemons. Each daemon gets its own local task.
    ///
    /// Does not wait for the daemons. If a daemon fails to start, its task
    /// tries a restart. The failure does not become an error here.
    /// Args:
    ///  - `specs`: Daemons to start
    ///  - `uid`, `gid`: Container user and group that run the daemons
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

    /// Get the current state of every daemon, sorted by name. Used by the
    /// `pollDaemons` RPC.
    pub fn snapshot(&self) -> Vec<(String, DaemonState)> {
        self.states
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect()
    }

    /// Tell every running daemon to stop. Each daemon task then does a
    /// [`graceful_stop`]. A second call does nothing.
    pub fn shutdown_all(&self) {
        let mut stops = self.stops.borrow_mut();
        for (_, tx) in std::mem::take(&mut *stops) {
            let _ = tx.send(());
        }
    }
}

/// Run one daemon with its restart loop until it stops, and keep its entry
/// in `states` up to date.
///
/// Each restart waits one second more than the one before.
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

/// Reason why [`run_daemon`] stopped waiting for the child.
enum Exit {
    Code(i32),
    StopRequested,
}

/// Stop a daemon child process.
///
/// Sends `signal` and waits up to `timeout_ms`. If the child is still alive
/// after that, sends SIGKILL.
/// Args:
///  - `child`: Daemon process
///  - `signal`: Stop signal to send first
///  - `timeout_ms`: Time to wait before SIGKILL. 0 means wait forever and
///    never send SIGKILL.
///
/// Returns:
///   `true` only if SIGKILL was sent (the daemon state is then `Killed`,
///   not `Stopped`).
async fn graceful_stop(mut child: tokio::process::Child, signal: i32, timeout_ms: u32) -> bool {
    let Some(pid) = child.id() else {
        // The child already exited. There is nothing to signal.
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

/// Create or truncate the stdout and stderr log files of a daemon.
///
/// Opened once per sandbox run. Every daemon restart duplicates the same
/// FD, so the output of a restart goes after the output before it. A new
/// sandbox run truncates the files.
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
