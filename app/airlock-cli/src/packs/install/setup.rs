//! Pack install boot.
//!
//! Boots the sandbox with the install config, runs the install script of
//! each pack, and shuts the VM down. A failed pack does not stop the other
//! packs. The install state is saved after each pack.

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

/// Stop a pack install if it prints nothing for this long.
const IDLE_TIMEOUT: Duration = Duration::from_mins(20);
/// After Ctrl+C or the idle timeout: time that the script gets to stop
/// after SIGTERM.
const TERM_GRACE: Duration = Duration::from_secs(10);
/// After SIGKILL: time to wait for the last output of the script.
const KILL_DRAIN: Duration = Duration::from_secs(5);

/// Reason why the install boot did not run.
#[derive(Debug, thiserror::Error)]
pub enum SetupError {
    /// The user pressed Ctrl+C during the boot.
    #[error("interrupted")]
    Interrupted,
    /// Another error.
    #[error(transparent)]
    Failed(anyhow::Error),
}

/// How the install process of one pack ended.
#[derive(Debug)]
pub enum Ended {
    /// The process exited with this exit code.
    Exited(i32),
    /// The user pressed Ctrl+C.
    Interrupted,
    /// The process printed nothing for [`IDLE_TIMEOUT`].
    TimedOut,
    /// The process could not be started.
    ExecFailed(anyhow::Error),
}

impl Ended {
    /// Explain why the loop stopped, for the user.
    /// Returns:
    ///   The reason, or `None` for an exit.
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

/// Runs the install process of one pack and sends its output to the
/// progress. The real implementation is [`VmExec`]. Tests use a fake.
pub trait Exec {
    /// Run the install process of `installer`.
    /// Returns:
    ///   How the process ended, or `None` if it did not start (Ctrl+C came
    ///   first).
    async fn run(
        &mut self,
        installer: &InstallerScript,
        progress: &mut InstallProgress,
    ) -> Option<Ended>;
}

/// Result of the install loop.
#[derive(Debug, Default)]
pub struct LoopOutcome {
    /// Packs whose script exited 0 in this boot.
    pub succeeded: Vec<String>,
    /// Packs whose script exited non-zero, with the exit code.
    pub failed: Vec<(String, i32)>,
    /// The process end that stopped the loop (Ctrl+C, idle timeout or a
    /// failed spawn), if any.
    pub stopped: Option<Ended>,
}

/// Callback that saves the state after each change. It sets the disk and
/// the image first.
pub type Save<'a> = dyn FnMut(&mut InstallState) -> anyhow::Result<()> + 'a;

/// Run the installers in order.
///
/// Packs are independent: a failed pack does not stop the loop. Other
/// process ends (Ctrl+C, idle timeout, a failed spawn) stop the loop.
/// Args:
///  - `exec`: Runs each install process
///  - `installers`: Installers to run, in order
///  - `state`: Install state to update. Exit 0 sets `unconfirmed`, other
///    ends set `failed`.
///  - `save`: Saves `state` after each installer. Thus a crash leaves a
///    state that the next start acts on.
///  - `progress`: Progress output
///
/// Returns:
///   What the loop did, or an error if a save failed.
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
            // Nothing ran. The record of the pack stays as it was.
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

/// Update the state after the VM shutdown.
///
/// With a confirmed disk sync, the packs that exited 0 in this boot become
/// `installed`, no matter what ended the loop. Other `unconfirmed` records
/// (from an earlier boot that did not confirm its sync) stay, because
/// those packs did not run in this boot. Without a sync, nothing changes,
/// and the next start runs those packs again.
/// Args:
///  - `state`: Install state to update
///  - `synced`: True if the guest confirmed its disk sync at shutdown
///  - `succeeded`: Packs whose script exited 0 in this boot
///  - `save`: Saves `state`
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

/// Result of the install boot.
pub struct Report {
    /// Result of the install loop.
    pub outcome: LoopOutcome,
    /// True if the guest confirmed its disk sync at shutdown.
    pub synced: bool,
}

/// Run the install boot.
///
/// Boots quietly without the project share, runs the installers in order
/// (one process each) and shuts down. The VM shuts down no matter what
/// ends the loop. The output goes to a spinner and to [`INSTALLS_LOG`].
/// For each failed pack, the last log lines go to stderr.
/// Args:
///  - `project`: Project, configured with [`phase::install_config`]
///  - `image`: Prepared image
///  - `installers`: Installers to run, in order
///  - `state`: Install state to update
///  - `save`: Saves `state` after each change
///  - `log_level`: Log level of the boot
///  - `verbose`: Show the last log lines under the spinner
///
/// Returns:
///   The report, or an error if the boot was interrupted or failed.
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

/// Boot the install VM. It has no network services, browser, clipboard,
/// daemons or masks, because the install config grants none of them. Its
/// network reaches only public addresses.
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

/// Path of the install log in the sandbox directory `sandbox_dir`.
pub fn log_path(sandbox_dir: &Path) -> std::path::PathBuf {
    sandbox_dir.join(INSTALLS_LOG)
}

/// [`Exec`] that uses [`Vm::spawn`]. The process runs as root (the image
/// user), in `/`, with pipes and a closed stdin.
struct VmExec<'a> {
    vm: &'a Vm,
}

impl Exec for VmExec<'_> {
    /// Start the script and run it to its end: exit, Ctrl+C (SIGTERM,
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
    /// Spawn the script and wait for its end.
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

/// Remove the old install log and open a new one. Logging is best effort.
fn open_log(project: &Project) -> Option<std::fs::File> {
    let dir = PinnedDir::pin(&project.sandbox_dir).ok()?;
    dir.remove(INSTALLS_LOG).ok()?;
    dir.open_append(INSTALLS_LOG, 0o600).ok()
}
