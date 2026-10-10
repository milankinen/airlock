//! The setup wizard of `airlock start`.
//!
//! Runs when the project has no config. Asks the user for packs and settings,
//! and makes a new project config from the answers. The config is saved only
//! after the sandbox is ready.

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
use crate::packs::{ConfiguredPack, PackManager};
use crate::start::wizard::form::Form;
use crate::vault::Vault;

/// All answers of the wizard.
pub struct Answers {
    /// The selected packs at their newest version, with the answered args.
    /// In pack order: the distro pack, the agents, the tools.
    pub packs: Vec<ConfiguredPack>,
    /// The image of the user files, if the user selected it instead of a
    /// distro pack (`vm.image` as the file has it).
    pub image: Option<serde_json::Value>,
    /// The clipboard capabilities (copy, paste).
    pub clipboard: Clipboard,
    /// Where the new config file goes (local or shareable).
    pub target: Target,
}

impl Answers {
    /// Return the entries of the new file: the selected packs with all their
    /// args.
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

    /// Return the new config file of the answers for the project `host_cwd`.
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

/// Inputs of the wizard.
pub(super) struct Input<'a> {
    /// Project directory on the host.
    pub(super) host_cwd: &'a Path,
    /// Airlock data directory, for the pack mount directories.
    pub(super) data_dir: &'a Path,
    /// Available packs.
    pub(super) packs: &'a PackManager,
    /// The user files (the project has no config yet). The answers must
    /// resolve with them (see [`check_answers`]).
    pub(super) config: &'a LayeredConfig,
    /// The vault that resolves `[env]` (see [`check_answers`]).
    pub(super) vault: &'a Vault,
}

/// Load the config files of the project, or run the setup wizard if the
/// project has none (see [`LayeredConfig::has_project_config`]).
/// Args:
///  - `host_cwd`: Project directory on the host
///  - `data_dir`: Airlock data directory
///  - `local_dir`: Directory of the local project config (see
///    [`crate::sandboxes::local_config_dir`])
///  - `has_sandbox`: The project has a sandbox with a disk or install
///    records (see [`crate::sandboxes::has_content`])
///  - `packs`: Available packs
///  - `vault`: Vault that resolves the `[env]` of the answers
///
/// Returns:
///   The loaded config. After the wizard, the config contains the answers as
///   the generated project config ([`LayeredConfig::with_generated_project`]).
///   The caller saves it after the sandbox is stored (see [`save_config`]).
pub async fn load_or_generate_config(
    host_cwd: &Path,
    data_dir: &Path,
    local_dir: &Path,
    has_sandbox: bool,
    packs: &PackManager,
    vault: &Vault,
) -> Result<LayeredConfig, Exit> {
    let config = config::load(host_cwd, local_dir).map_err(Exit::config)?;
    if config.has_project_config() {
        return Ok(config);
    }
    if has_sandbox {
        cli::error!(
            "A sandbox exists in {}, but there is no config. Put the config in airlock.toml or \
             {} (restore it), or run `airlock rm` to start over.",
            host_cwd.display(),
            local_dir.join("airlock.toml").display()
        );
        return Err(Exit::Code(2));
    }
    let generated = Box::pin(run_wizard(host_cwd, data_dir, packs, &config, vault)).await?;
    config
        .with_generated_project(generated)
        .map_err(Exit::config)
}

/// Run the setup wizard for a project without config.
///
/// The wizard is one view with the packs grouped by kind
/// ([`PackKind`](crate::packs::PackKind)):
///  * The distro pack, or the image of the user files
///  * The agents and the tools
///  * The args of each selected pack
///  * The clipboard capabilities (copy, paste)
///  * The start bar: `start` (local config, in the sandbox directory or in
///    `.airlock/`), `start and share` (shareable config, `airlock.toml`) or
///    `cancel`. The bar is on `start and share` first.
///
/// The user files do not select packs, because `[packs]` belongs in the
/// project files. If the user files set an image, the distro group starts
/// with it (`custom (<image>)`, no distro pack) and it is selected. Otherwise
/// the first distro pack is selected. No agent or tool is selected, and the
/// args have their defaults.
///
/// On a start option, the answers must resolve with the user files, also
/// `[env]` (see [`check_answers`]). If they do not, the error shows in the
/// view, and the view stays open. The wizard writes nothing and takes no
/// lock.
/// Args:
///  - `host_cwd`: Project directory on the host
///  - `data_dir`: Airlock data directory
///  - `packs`: Available packs
///  - `config`: The user config files only
///  - `vault`: Vault that resolves the `[env]` of the answers
///
/// Returns:
///   The new config file (not saved yet, see [`crate::config::generated`]).
///   It has the selected packs at their newest version with all their args,
///   and `[clipboard]`. The distro pack sets the image, or the file repeats
///   the user image. Errors:
///    * No terminal: exit code 2
///    * Esc or `cancel`: exit code 0, or 2 after a failed check (the error
///      stays valid)
///    * Ctrl+C: exit code 130
pub async fn run_wizard(
    host_cwd: &Path,
    data_dir: &Path,
    packs: &PackManager,
    config: &LayeredConfig,
    vault: &Vault,
) -> Result<GeneratedConfig, Exit> {
    if !prompt::can_prompt() {
        cli::error!(
            "No airlock config in {}. Run `airlock start` in a terminal, or create airlock.toml.",
            host_cwd.display()
        );
        return Err(Exit::Code(2));
    }
    let input = Input {
        host_cwd,
        data_dir,
        packs,
        config,
        vault,
    };
    Box::pin(ask_config(&input)).await
}

/// Save the config of the wizard. Call this after the sandbox is stored.
/// The local file goes to `local_dir` (see
/// [`crate::sandboxes::local_config_dir`]).
// Report only a shareable file (`airlock.toml`). The user must not edit or
// commit the local file.
pub fn save_config(generated: &GeneratedConfig, local_dir: &Path) -> Result<(), Exit> {
    generated.save(local_dir).map_err(|e| Exit::error(1, e))?;
    if generated.target == Target::Project {
        cli::log!("  {} created {}", cli::check(), generated.path.display());
    }
    Ok(())
}

/// How the view ended.
enum End {
    /// A start option, with answers that passed [`check_answers`].
    Done(Answers),
    /// Esc or `cancel`.
    Cancelled,
    /// Ctrl+C or an interrupt signal.
    Interrupted,
}

/// Show the view until it ends (see [`run_wizard`]) and return the new
/// config. Messages print after the terminal is restored.
async fn ask_config(input: &Input<'_>) -> Result<GeneratedConfig, Exit> {
    let mut form = Form::new(input.data_dir, input.packs, input.config.user_image());
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
                // Leave raw mode and remove the view. Then the output of the
                // check (a vault passphrase prompt) shows as usual.
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

/// Check the config of `answers` as the rest of `airlock start` uses it.
///
/// With the user files, the config must resolve (see
/// [`LayeredConfig::resolve`]). Each pack `config.lua` must run, and no two
/// packs can set a value differently. The `[env]` of the config must also
/// resolve (see [`crate::project::resolve_env`]): each host variable that it
/// names must be set.
pub(super) async fn check_answers(input: &Input<'_>, answers: &Answers) -> anyhow::Result<()> {
    let config = input
        .config
        .clone()
        .with_generated_project(answers.config(input.host_cwd))?;
    let resolved = config
        .resolve(
            input.data_dir,
            input.packs,
            &config::ConfigOverrides::default(),
        )
        .await?;
    crate::project::resolve_env(&resolved.values, input.vault)?;
    Ok(())
}
