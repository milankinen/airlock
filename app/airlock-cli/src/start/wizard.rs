//! The setup wizard of `airlock start`, for a project without config.
//!
//! It runs only when the project has no sandbox and there is a terminal.
//! It is one view (its state: [`form`]; its lines: [`view`]; drawn on
//! the [`screen`](crate::cli::prompt::screen)) with the packs by their kind
//! ([`PackKind`](crate::packs::PackKind)): the distro pack (or the
//! user's own image), the agents and the tools, the args of each
//! selected pack, the clipboard capabilities (copy, paste), and the
//! start bar: `start` (the config is local, `.airlock/airlock.toml`),
//! `start and share` (it is shareable, `airlock.toml`) or `cancel`. It
//! returns the new file ([`crate::config::generated`]) with the chosen
//! packs at their newest version and all their args, and `[clipboard]`;
//! the distro pack sets the image, or the new file repeats the user's
//! own image (`vm.image` as the user file has it). The rest
//! of `airlock start` then runs with that config as with any other; the
//! file is saved ([`save_config`]) once the sandbox is stored. The wizard
//! writes nothing and takes no lock.
//!
//! No pack is pre-selected from the user files (`[packs]` belongs in the
//! project files). When the user files set an image, the distro group
//! starts with it (`custom (<image>)`, no distro pack) and it is
//! selected; else the first distro pack is. No agent or tool is selected,
//! and the args have their defaults. On a start option the answers must
//! resolve with the user files, `[env]` too (see [`check_answers`]); if
//! they do not, the error shows in the view, which stays open. Esc or
//! `cancel` ends the run
//! without a config (exit code 0, or 2 after a failed check: the error
//! stands); Ctrl-C ends it with exit code 130.

pub mod form;
pub mod view;

use std::path::Path;

use super::Exit;
use crate::cli;
use crate::cli::prompt;
use crate::cli::prompt::Step;
use crate::cli::prompt::screen::{self, Screen};
use crate::config::generated::{Clipboard, GeneratedConfig, NewEntry, Target};
use crate::config::{self, LayeredConfig};
use crate::packs::install::state;
use crate::packs::{ConfiguredPack, PackManager};
use crate::start::wizard::form::Form;
use crate::vault::Vault;
use crate::vm::disk;

/// Everything the wizard asks.
pub struct Answers {
    /// The chosen packs at their newest version, with the answered args,
    /// in pack order: the distro pack, the agents, the tools.
    pub packs: Vec<ConfiguredPack>,
    /// The image of the user files, when it is chosen instead of a
    /// distro pack (`vm.image` as the file has it).
    pub image: Option<serde_json::Value>,
    pub clipboard: Clipboard,
    pub target: Target,
}

impl Answers {
    /// The entries of the new file: the chosen packs with all their args.
    fn entries(&self) -> Vec<NewEntry<'_>> {
        self.packs
            .iter()
            .map(|c| {
                let metadata = c.metadata();
                NewEntry {
                    name: &metadata.name,
                    version: &metadata.version,
                    args: c
                        .args()
                        .iter()
                        .map(|(key, value)| (key.as_str(), value))
                        .collect(),
                }
            })
            .collect()
    }

    /// The new config file of the answers in the project `host_cwd`.
    pub fn config(&self, host_cwd: &Path) -> GeneratedConfig {
        GeneratedConfig::new(
            host_cwd,
            self.target,
            &self.entries(),
            self.image.as_ref(),
            Some(self.clipboard),
        )
    }
}

/// What the wizard works with.
pub(super) struct Input<'a> {
    pub(super) host_cwd: &'a Path,
    pub(super) packs: &'a PackManager,
    /// The user files (the project has no config yet), with which the
    /// answers must resolve (see [`check_answers`]).
    pub(super) config: &'a LayeredConfig,
    /// The vault that resolves `[env]` (see [`check_answers`]).
    pub(super) vault: &'a Vault,
}

/// Load the project's config files, or (a project without one, see
/// [`LayeredConfig::has_project_config`]) run the setup wizard and carry
/// its answer as the generated project config
/// ([`LayeredConfig::with_generated_project`]). The caller saves it once
/// the sandbox is stored (see [`save_config`]). `vault` resolves the
/// `[env]` of the answers (see [`check_answers`]).
pub async fn load_or_generate_config(
    host_cwd: &Path,
    packs: &PackManager,
    vault: &Vault,
) -> Result<LayeredConfig, Exit> {
    let config = config::load().map_err(Exit::config)?;
    if config.has_project_config() {
        return Ok(config);
    }
    let generated = Box::pin(run_wizard(host_cwd, packs, &config, vault)).await?;
    config
        .with_generated_project(generated)
        .map_err(Exit::config)
}

/// The setup wizard for the project at `host_cwd` without config
/// (`config` has the user files only). A sandbox without config and a
/// missing terminal are errors (exit code 2). `vault` resolves the
/// `[env]` of the answers (see [`check_answers`]). Returns the new config
/// (not saved yet); see the module docs for how the view ends.
pub async fn run_wizard(
    host_cwd: &Path,
    packs: &PackManager,
    config: &LayeredConfig,
    vault: &Vault,
) -> Result<GeneratedConfig, Exit> {
    if sandbox_exists(host_cwd) {
        cli::error!(
            "A sandbox exists in {}, but there is no config. Put the config in airlock.toml or \
             .airlock/airlock.toml (restore it), or run `airlock rm` to start over.",
            host_cwd.display()
        );
        return Err(Exit::Code(2));
    }
    if !prompt::can_prompt() {
        cli::error!(
            "No airlock config in {}. Run `airlock start` in a terminal, or create airlock.toml.",
            host_cwd.display()
        );
        return Err(Exit::Code(2));
    }
    let input = Input {
        host_cwd,
        packs,
        config,
        vault,
    };
    Box::pin(ask_config(&input)).await
}

/// Save the config of the wizard (after the sandbox is stored). Only a
/// shareable file (`airlock.toml`) is reported: the local one is not for
/// the user to edit or commit.
pub fn save_config(generated: &GeneratedConfig) -> Result<(), Exit> {
    generated.save().map_err(|e| Exit::error(1, e))?;
    if generated.target == Target::Project {
        cli::log!("  {} created {}", cli::check(), generated.path.display());
    }
    Ok(())
}

/// Whether the project at `host_cwd` has a sandbox: a disk or an install
/// state in `.airlock/sandbox`.
fn sandbox_exists(host_cwd: &Path) -> bool {
    let dir = host_cwd.join(".airlock/sandbox");
    dir.join(disk::DISK_FILE).exists() || dir.join(state::STATE_FILE).exists()
}

/// How the view ended.
enum End {
    /// A start option, with answers that passed [`check_answers`].
    Done(Answers),
    Cancelled,
    Interrupted,
}

/// Show the view until it ends (see the module docs) and return the new
/// config. Messages print after the terminal is restored.
async fn ask_config(input: &Input<'_>) -> Result<GeneratedConfig, Exit> {
    let mut form = Form::new(input.packs, input.config.user_image());
    let mut screen = Screen::open()?;
    let end = Box::pin(run_view(input, &mut form, &mut screen)).await;
    let closed = screen.close();
    let end = end?;
    closed?;
    match end {
        End::Done(answers) => Ok(answers.config(input.host_cwd)),
        End::Cancelled => {
            if let Some(e) = form.check_error() {
                return Err(Exit::config(e));
            }
            cli::error!("Aborted; no config written.");
            Err(Exit::Code(0))
        }
        End::Interrupted => Err(Exit::INTERRUPTED),
    }
}

/// Draw `form` on `screen` and apply the keys until the view ends.
async fn run_view(
    input: &Input<'_>,
    form: &mut Form,
    screen: &mut Screen,
) -> Result<End, prompt::PromptError> {
    loop {
        screen.draw(|room| view::frame(form, room))?;
        let key = match screen::next_input().await? {
            screen::Input::Key(key) => key,
            screen::Input::Resized => continue,
            screen::Input::Interrupted => return Ok(End::Interrupted),
        };
        match form.key(key) {
            Step::Stay => {}
            Step::Cancel => return Ok(End::Cancelled),
            Step::Interrupt => return Ok(End::Interrupted),
            Step::Done(target) => {
                let answers = form.answers(target);
                // Out of raw mode and without the view: what the check
                // prints (a vault passphrase prompt) shows as usual.
                screen.suspend()?;
                match check_answers(input, &answers).await {
                    Ok(()) => return Ok(End::Done(answers)),
                    Err(e) => {
                        form.check_failed(format!("{e:#}"));
                        screen.resume()?;
                    }
                }
            }
        }
    }
}

/// Check the config of `answers` as the rest of `airlock start` uses it:
/// with the user files, it resolves (each pack's `config.lua`, and no two
/// packs set a value differently, see [`LayeredConfig::resolve`]), and so
/// does its `[env]` (each host variable that it names is set, see
/// [`crate::project::resolve_env`]).
pub(super) async fn check_answers(input: &Input<'_>, answers: &Answers) -> anyhow::Result<()> {
    let config = input
        .config
        .clone()
        .with_generated_project(answers.config(input.host_cwd))?;
    let resolved = config
        .resolve(input.packs, &config::ConfigOverrides::default())
        .await?;
    crate::project::resolve_env(&resolved.values, input.vault)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_exists_with_disk_or_install_state_only() {
        let tmp = crate::test_cfg::temp_dir();
        let dir = tmp.path().join(".airlock/sandbox");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("lock"), "").unwrap();
        assert!(!sandbox_exists(tmp.path()));
        std::fs::write(dir.join(state::STATE_FILE), "{}").unwrap();
        assert!(sandbox_exists(tmp.path()));
        std::fs::remove_file(dir.join(state::STATE_FILE)).unwrap();
        std::fs::write(dir.join(disk::DISK_FILE), "").unwrap();
        assert!(sandbox_exists(tmp.path()));
    }
}
