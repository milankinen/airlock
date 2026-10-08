//! The install boot: boot with the install config, spawn one process per
//! pack, and shut down cleanly.
//!
//! Packs are independent: a failed pack does not stop the next one. After
//! each process the state is saved (exit 0 → `unconfirmed`, else `failed`),
//! so a crash leaves a state that the next start acts on. Whatever ends
//! the loop (all done, Ctrl+C, idle timeout, a process that cannot start),
//! the VM shuts down; only when the guest confirmed its disk sync do the
//! `unconfirmed` records become `installed`.

use std::path::Path;
use std::time::{Duration, Instant};

use super::progress::{INSTALLS_LOG, InstallProgress};
use super::state::{self, InstallState, PackStatus};
use super::{compose, phase};
use crate::cli::{self, LogLevel};
use crate::network::{self, Network};
use crate::oci::{self, OciImage};
use crate::packs::InstallerScript;
use crate::project::Project;
use crate::sandbox::boot::{self, BootSpec};
use crate::sandbox::vm::{ProcessSpec, Stopped, Vm};
use crate::sandbox::{self, io};
use crate::util::PinnedDir;

/// Give up on a pack when it prints nothing for this long.
const IDLE_TIMEOUT: Duration = Duration::from_mins(20);
/// After Ctrl+C: how long the script gets to stop after SIGTERM.
const TERM_GRACE: Duration = Duration::from_secs(10);
/// After SIGKILL: how long its last output may take to arrive.
const KILL_DRAIN: Duration = Duration::from_secs(5);

/// Why the install boot did not run.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    #[error("interrupted")]
    Interrupted,
    #[error(transparent)]
    Failed(anyhow::Error),
}

/// How one pack's process ended.
#[derive(Debug)]
pub enum Ended {
    Exited(i32),
    Interrupted,
    TimedOut,
    /// The process could not be started.
    ExecFailed(anyhow::Error),
}

impl Ended {
    /// Why the loop stopped, for the user; `None` for an exit.
    pub fn reason(&self) -> Option<String> {
        match self {
            Ended::Exited(_) => None,
            Ended::Interrupted => Some("interrupted".into()),
            Ended::TimedOut => Some(format!(
                "the install printed nothing for {} minutes and was stopped",
                IDLE_TIMEOUT.as_secs() / 60
            )),
            Ended::ExecFailed(e) => Some(format!("could not start the install: {e:#}")),
        }
    }
}

/// Runs one pack's install process, feeding its output to the progress.
/// The real one is [`VmExec`]; tests use a fake. `None`: the exec did not
/// start (Ctrl+C came first).
pub trait Exec {
    async fn run(
        &mut self,
        installer: &InstallerScript,
        progress: &mut InstallProgress,
    ) -> Option<Ended>;
}

/// What the install loop did.
#[derive(Debug, Default)]
pub struct LoopOutcome {
    /// Packs whose script exited 0 in this boot.
    pub succeeded: Vec<String>,
    /// Packs whose script exited non-zero, with the exit code.
    pub failed: Vec<(String, i32)>,
    /// What stopped the loop before the last pack, if anything.
    pub stopped: Option<Ended>,
}

/// Saves the state after each change (sets the disk and image first).
pub type Save<'a> = dyn FnMut(&mut InstallState) -> anyhow::Result<()> + 'a;

/// Run `installers` in order through `exec`, saving `state` after each
/// one. A failed pack does not stop the loop; anything else that ends a
/// process (Ctrl+C, idle timeout, a failed spawn) does.
pub async fn install_loop<E: Exec>(
    exec: &mut E,
    installers: &[InstallerScript],
    state: &mut InstallState,
    save: &mut Save<'_>,
    progress: &mut InstallProgress,
) -> anyhow::Result<LoopOutcome> {
    let mut outcome = LoopOutcome::default();
    for installer in installers {
        let name = installer.pack.as_str();
        let fingerprint = &installer.fingerprint;
        progress.begin(name, &installer.label);
        let Some(ended) = exec.run(installer, progress).await else {
            // Nothing ran: the pack's record stays as it was.
            progress.end(" not started", false);
            outcome.stopped = Some(Ended::Interrupted);
            break;
        };
        match ended {
            Ended::Exited(0) => {
                state.set(name, PackStatus::Unconfirmed, fingerprint);
                progress.end("", true);
                outcome.succeeded.push(name.to_string());
            }
            Ended::Exited(code) => {
                state.set(name, PackStatus::Failed, fingerprint);
                progress.end(&format!(" failed (exit code {code})"), false);
                progress.print_tail();
                outcome.failed.push((name.to_string(), code));
            }
            other => {
                state.set(name, PackStatus::Failed, fingerprint);
                progress.end(" stopped", false);
                if !matches!(other, Ended::Interrupted) {
                    progress.print_tail();
                }
                outcome.stopped = Some(other);
            }
        }
        save(state)?;
        if outcome.stopped.is_some() {
            break;
        }
    }
    Ok(outcome)
}

/// After the shutdown: with a confirmed disk sync, the `unconfirmed`
/// records of the packs that exited 0 in this boot (`succeeded`) become
/// `installed`, whatever ended the loop. Other `unconfirmed` records (an
/// earlier boot that did not confirm its sync) stay: those packs did not
/// run in this boot. Without a sync, nothing is promoted (the next start
/// runs those packs again).
pub fn finish(
    state: &mut InstallState,
    synced: bool,
    succeeded: &[String],
    save: &mut Save<'_>,
) -> anyhow::Result<()> {
    if synced {
        state::promote(state, succeeded);
        save(state)?;
    }
    Ok(())
}

/// What the install boot left.
pub struct Report {
    pub outcome: LoopOutcome,
    /// The guest confirmed its disk sync at shutdown.
    pub synced: bool,
}

/// Run the install boot for `project` (configured with
/// [`phase::install_config`]): boot quietly without the project share,
/// run `installers` in order, one process each, and shut down. The output
/// goes to a spinner, [`INSTALLS_LOG`] and, per failed pack, a tail on
/// stderr.
pub async fn install(
    project: Project,
    image: &OciImage,
    installers: &[InstallerScript],
    state: &mut InstallState,
    save: &mut Save<'_>,
    log_level: LogLevel,
    verbose: bool,
) -> Result<Report, SetupError> {
    let log = open_log(&project);
    let mut progress = InstallProgress::new(cli::spinner(""), log, verbose);
    let vm = match Box::pin(boot_install_vm(project, image, log_level)).await {
        Ok(vm) => vm,
        Err(e) => {
            progress.finish();
            if e.is::<sandbox::Interrupted>() {
                return Err(SetupError::Interrupted);
            }
            return Err(SetupError::Failed(e.context("boot the install VM")));
        }
    };

    let mut exec = VmExec { vm: &vm };
    let outcome = install_loop(&mut exec, installers, state, save, &mut progress).await;
    progress.finish();

    let Stopped { synced, .. } = Box::pin(vm.shutdown()).await.map_err(SetupError::Failed)?;
    let outcome = outcome.map_err(SetupError::Failed)?;
    finish(state, synced, &outcome.succeeded, save).map_err(SetupError::Failed)?;
    Ok(Report { outcome, synced })
}

/// Boot the install VM: no network services, browser, clipboard, daemons
/// or masks (the install config grants none of them), and a network that
/// reaches public addresses only.
async fn boot_install_vm(
    project: Project,
    image: &OciImage,
    log_level: LogLevel,
) -> anyhow::Result<Vm> {
    let container_home = oci::effective_container_home(&project, image);
    let network = Network::new(
        &project,
        &container_home,
        network::native_tls_client(),
        vec![],
        vec![],
    )?
    .public_only();
    let env = boot::guest_env(&project, image, false);
    boot::boot(BootSpec {
        project,
        image,
        options: phase::boot_options(log_level),
        env,
        network,
        browser: None,
        clipboard: None,
        daemons: vec![],
        masks: vec![],
    })
    .await
}

/// The install log in `sandbox_dir`.
pub fn log_path(sandbox_dir: &Path) -> std::path::PathBuf {
    sandbox_dir.join(INSTALLS_LOG)
}

/// [`Exec`] over [`Vm::spawn`]: as root (the image user), in `/`, with
/// pipes and a closed stdin.
struct VmExec<'a> {
    vm: &'a Vm,
}

impl Exec for VmExec<'_> {
    /// Start the script and drive it to its end: exit, Ctrl+C (SIGTERM,
    /// then SIGKILL), or no output for [`IDLE_TIMEOUT`].
    async fn run(
        &mut self,
        installer: &InstallerScript,
        progress: &mut InstallProgress,
    ) -> Option<Ended> {
        if cli::is_interrupted() {
            return None;
        }
        Some(self.spawn(installer, progress).await)
    }
}

impl VmExec<'_> {
    async fn spawn(&self, installer: &InstallerScript, progress: &mut InstallProgress) -> Ended {
        let spec = ProcessSpec {
            argv: compose::argv(installer),
            cwd: Some("/".into()),
            env: installer.env.clone(),
            unset: vec![],
            stdin: io::closed_stdin(),
            pty: None,
        };
        let proc = match self.vm.spawn(spec).await {
            Ok(proc) => proc,
            Err(e) => return Ended::ExecFailed(e),
        };

        let activity = progress.activity();
        let idle = async {
            loop {
                let deadline = activity.get() + IDLE_TIMEOUT;
                tokio::time::sleep_until(deadline.into()).await;
                if Instant::now() >= activity.get() + IDLE_TIMEOUT {
                    return;
                }
            }
        };
        let mut run = std::pin::pin!(io::drive(&proc, progress));
        let ended = tokio::select! {
            code = &mut run => return Ended::Exited(code),
            () = idle => Ended::TimedOut,
            () = cli::interrupted() => Ended::Interrupted,
        };
        // Stop the script: SIGTERM, a grace period, then SIGKILL.
        let _ = proc.signal(libc::SIGTERM).await;
        if tokio::time::timeout(TERM_GRACE, &mut run).await.is_err() {
            let _ = proc.kill().await;
            let _ = tokio::time::timeout(KILL_DRAIN, &mut run).await;
        }
        ended
    }
}

/// Truncate and open the install log. Logging is best effort.
fn open_log(project: &Project) -> Option<std::fs::File> {
    let dir = PinnedDir::open(&project.host_cwd, Path::new(".airlock/sandbox"), false).ok()?;
    dir.remove(INSTALLS_LOG).ok()?;
    dir.open_append(INSTALLS_LOG, 0o600).ok()
}
