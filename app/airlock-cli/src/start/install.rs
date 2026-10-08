//! The install step of `airlock start`: run the install boot
//! ([`setup::install`]) for the tools that [`super::sandbox::ensure_sandbox`]
//! decided to install, and save their records. No questions: a failed
//! install ends the run (exit code 1), and the next start installs it
//! again.

use super::{Exit, SandboxOptions};
use crate::cli;
use crate::cli::prompt;
use crate::config::ResolvedConfig;
use crate::oci::OciImage;
#[cfg(not(test))]
use crate::packs::install::setup::install as install_boot;
use crate::packs::install::state::{self as install_state, InstallState};
use crate::packs::install::{compose, plan, setup};
use crate::packs::{ConfiguredPack, InstallerScript};
use crate::project::{self, Project};
#[cfg(test)]
use crate::test_cfg::start::install_boot;
use crate::util::PinnedDir;

/// What [`install_tools`] installs: decided (and the records saved) by
/// [`super::sandbox::ensure_sandbox`].
pub struct InstallDecision {
    /// `.airlock/sandbox/`.
    sandbox: PinnedDir,
    /// The records, as saved.
    state: InstallState,
    /// The installs to run now, in pack order.
    to_install: Vec<InstallerScript>,
}

impl InstallDecision {
    /// Apply `transitions` to `state`, save it when it changed or
    /// something installs, and install `to_install`.
    pub(super) fn save(
        sandbox: PinnedDir,
        mut state: InstallState,
        transitions: &[plan::Transition],
        to_install: Vec<InstallerScript>,
        image: &OciImage,
    ) -> anyhow::Result<Self> {
        let before = state.clone();
        install_state::apply(&mut state, transitions);
        if state != before || !to_install.is_empty() {
            save_state(&sandbox, &image.image_id, &mut state)?;
        }
        Ok(Self {
            sandbox,
            state,
            to_install,
        })
    }
}

/// The installs of the packs whose version has a setup script.
pub fn install_candidates(configured: &[ConfiguredPack]) -> Vec<InstallerScript> {
    configured
        .iter()
        .filter_map(ConfiguredPack::setup_installer)
        .collect()
}

/// Run the install boot for the decided tools (its own project, with
/// [`ResolvedConfig::install_config`]; `project`'s run config is
/// untouched), and save the records. A pack that still has no
/// `installed` record after this ends the run (exit code 1); config is
/// the source of truth for tools, so there is no starting without one.
pub async fn install_tools(
    project: &Project,
    resolved: &ResolvedConfig,
    image: &OciImage,
    decision: InstallDecision,
    options: &SandboxOptions,
) -> Result<(), Exit> {
    let InstallDecision {
        sandbox,
        mut state,
        to_install,
    } = decision;
    let install = !to_install.is_empty();
    if install {
        if cli::is_interrupted() {
            return Err(Exit::INTERRUPTED);
        }
        let install_project = project
            .with_config(resolved.install_config().map_err(Exit::config)?)
            .map_err(|e| Exit::error(1, e))?;
        // Saved with the first record: no session ran since this install.
        state.ran_session = false;
        let mut save = |s: &mut InstallState| save_state(&sandbox, &image.image_id, s);
        let run = run_install(
            install_project,
            image,
            &to_install,
            &mut state,
            &mut save,
            options,
        );
        Box::pin(run).await?;
    }

    if to_install.iter().any(|t| !state.is_installed(&t.pack)) {
        let log = setup::log_path(&project.sandbox_dir);
        eprint!("Full log: {}\r\n", log.display());
        return Err(Exit::Code(1));
    }
    if install {
        prompt::flush_input();
    }
    // The session runs next; a later retry asks first (see
    // [`InstallState::ran_session`]).
    if !state.ran_session && !state.packs.is_empty() {
        state.ran_session = true;
        save_state(&sandbox, &image.image_id, &mut state)?;
    }
    Ok(())
}

/// Save `state` with the current disk and `image_id`.
fn save_state(sandbox: &PinnedDir, image_id: &str, state: &mut InstallState) -> anyhow::Result<()> {
    state.disk = project::disk_id(sandbox.path());
    state.image_id = Some(image_id.to_string());
    install_state::write(sandbox, state)
}

/// Run the install boot for `to_install`, and report what failed.
async fn run_install(
    project: Project,
    image: &OciImage,
    to_install: &[InstallerScript],
    state: &mut InstallState,
    save: &mut setup::Save<'_>,
    options: &SandboxOptions,
) -> Result<(), Exit> {
    cli::log!("Installing packs...");
    let report = match Box::pin(install_boot(
        project,
        image,
        to_install,
        state,
        save,
        options.log_level,
        options.verbose,
    ))
    .await
    {
        Ok(r) => r,
        Err(setup::SetupError::Interrupted) => return Err(Exit::INTERRUPTED),
        Err(setup::SetupError::Failed(e)) => return Err(e.into()),
    };
    match &report.outcome.stopped {
        Some(setup::Ended::Interrupted) => return Err(Exit::INTERRUPTED),
        Some(stopped) => {
            if let Some(reason) = stopped.reason() {
                cli::error!("{reason}");
            }
        }
        None => {}
    }
    for (id, code) in &report.outcome.failed {
        let label = to_install
            .iter()
            .find(|t| &t.pack == id)
            .map_or(id.as_str(), |t| t.label.as_str());
        cli::error!("{label} failed (exit code {code})");
        if let Some(hint) = compose::exit_hint(*code) {
            cli::error!("hint: {hint}");
        }
    }
    if !report.synced {
        cli::error!(
            "The install VM did not confirm that its disk was written; the packs \
             install again on the next start"
        );
    }
    Ok(())
}
