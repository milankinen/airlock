//! The setup wizard of `airlock start` and `airlock templates create`.
//!
//! Runs when the project has no config. Asks the user for a template, packs
//! and settings, and makes a new project config from the answers. The config
//! is saved only after the sandbox is ready. The same view also makes the
//! config of a new template.

pub mod form;
pub mod view;

use super::Exit;
use crate::cli::prompt;
use crate::cli::prompt::Step;
use crate::cli::prompt::screen::{self, Screen};
use crate::config::generated::{Clipboard, GeneratedConfig, NewEntry, Target};
use crate::config::{self, LayeredConfig};
use crate::data_dir::DataDir;
use crate::packs::{ConfiguredPack, PackManager};
use crate::start::wizard::form::{Form, Mode};
use crate::templates::Template;
use crate::util::VerifiedPath;
use crate::vault::Vault;
use crate::{cli, messages};

/// All answers of the wizard.
pub struct Answers {
    /// The selected template, if any. The config starts from its config.
    pub template: Option<Template>,
    /// The selected packs at their newest version, with the answered args.
    /// In pack order: the distro pack, the agents, the tools. With a
    /// template, only the packs that the template does not have.
    pub packs: Vec<ConfiguredPack>,
    /// The image of the user files, if the user selected it instead of a
    /// distro pack (`vm.image` as the file has it). Never set with a
    /// template.
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
    pub fn config(&self, host_cwd: &VerifiedPath) -> GeneratedConfig {
        GeneratedConfig::new(
            host_cwd,
            self.target,
            &self.entries(),
            self.image.as_ref(),
            Some(self.clipboard),
            self.template.as_ref(),
        )
    }
}

/// Inputs of the wizard.
pub(super) struct Input<'a> {
    /// Project directory on the host.
    pub(super) host_cwd: &'a VerifiedPath,
    /// Airlock data directory, for the pack mount directories.
    pub(super) data_dir: &'a DataDir,
    /// Available packs.
    pub(super) packs: &'a PackManager,
    /// The user files (the project has no config yet). The answers must
    /// resolve with them (see [`check_answers`]).
    pub(super) config: &'a LayeredConfig,
    /// The vault that resolves `[env]` (see [`check_answers`]).
    pub(super) vault: &'a Vault,
    /// The templates, sorted by name.
    pub(super) templates: &'a [Template],
    /// What the wizard makes.
    pub(super) mode: Mode,
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
///  - `templates`: The templates that the wizard offers, sorted by name
///
/// Returns:
///   The loaded config. After the wizard, the config contains the answers as
///   the generated project config ([`LayeredConfig::with_generated_project`]).
///   The caller saves it after the sandbox is stored (see [`save_config`]).
pub async fn load_or_generate_config(
    host_cwd: &VerifiedPath,
    data_dir: &DataDir,
    local_dir: &VerifiedPath,
    has_sandbox: bool,
    packs: &PackManager,
    vault: &Vault,
    templates: &[Template],
) -> Result<LayeredConfig, Exit> {
    let config = config::load(host_cwd, local_dir).map_err(Exit::config)?;
    if config.has_project_config() {
        return Ok(config);
    }
    if has_sandbox {
        cli::error!(
            "{}",
            messages::start::sandbox_without_config(
                host_cwd,
                local_dir.given().join("airlock.toml").display()
            )
        );
        return Err(Exit::Code(2));
    }
    let generated = Box::pin(run_wizard(
        host_cwd, data_dir, packs, &config, vault, templates,
    ))
    .await?;
    config
        .with_generated_project(generated)
        .map_err(Exit::config)
}

/// Run the setup wizard for a project without config.
///
/// The wizard is one view with the packs grouped by kind
/// ([`PackKind`](crate::packs::PackKind)):
///  * The template, if templates exist: `empty` (selected at
///    first) or a template. A template selects and locks its packs and the
///    image (see [`Form`]).
///  * The distro pack, or the image of the user files
///  * The agents and the tools
///  * The args of each selected pack
///  * The clipboard capabilities (copy, paste)
///  * The config: `Create airlock.toml` (off at first). On: a shareable
///    config (`airlock.toml`). Off: a local config, in the sandbox directory
///    or in `.airlock/`.
///  * The start bar: `start` or `abort`
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
///  - `templates`: The templates that the wizard offers, sorted by name
///
/// Returns:
///   The new config file (not saved yet, see [`crate::config::generated`]).
///   It has the selected packs at their newest version with all their args,
///   and `[clipboard]`. The distro pack sets the image, or the file repeats
///   the user image. With a template, the file is the template config with
///   the other selected packs and `[clipboard]`. Errors:
///    * No terminal: exit code 2
///    * Esc or `abort`: exit code 0, or 2 after a failed check (the error
///      stays valid)
///    * Ctrl+C: exit code 130
pub async fn run_wizard(
    host_cwd: &VerifiedPath,
    data_dir: &DataDir,
    packs: &PackManager,
    config: &LayeredConfig,
    vault: &Vault,
    templates: &[Template],
) -> Result<GeneratedConfig, Exit> {
    if !prompt::can_prompt() {
        cli::error!("{}", messages::start::no_config_no_terminal(host_cwd));
        return Err(Exit::Code(2));
    }
    let input = Input {
        host_cwd,
        data_dir,
        packs,
        config,
        vault,
        templates,
        mode: Mode::Start,
    };
    Box::pin(ask_config(&input)).await
}

/// Run the setup wizard for a new template (`airlock templates create`).
///
/// The view is the view of [`run_wizard`], but without the config section,
/// and the start bar has `create` and `abort`. A new template can start
/// from a template.
/// Args:
///  - `host_cwd`: Directory of the command, for the config path
///  - `data_dir`: Airlock data directory
///  - `packs`: Available packs
///  - `config`: The user config files only
///  - `vault`: Vault that resolves the `[env]` of the answers
///  - `templates`: The templates that the wizard offers, sorted by name
///
/// Returns:
///   The config of the new template (local target, see [`run_wizard`]).
///   Errors:
///    * No terminal: exit code 2
///    * Esc or `abort`: exit code 0, or 2 after a failed check
///    * Ctrl+C: exit code 130
pub async fn run_template_wizard(
    host_cwd: &VerifiedPath,
    data_dir: &DataDir,
    packs: &PackManager,
    config: &LayeredConfig,
    vault: &Vault,
    templates: &[Template],
) -> Result<GeneratedConfig, Exit> {
    if !prompt::can_prompt() {
        cli::error!("{}", messages::start::TEMPLATE_NEEDS_TERMINAL);
        return Err(Exit::Code(2));
    }
    let input = Input {
        host_cwd,
        data_dir,
        packs,
        config,
        vault,
        templates,
        mode: Mode::Template,
    };
    Box::pin(ask_config(&input)).await
}

/// Save the config of the wizard. Call this after the sandbox is stored.
/// The local file goes to `local_dir` (see
/// [`crate::sandboxes::local_config_dir`]).
// Report only a shareable file (`airlock.toml`). The user must not edit or
// commit the local file.
pub fn save_config(generated: &GeneratedConfig, local_dir: &VerifiedPath) -> Result<(), Exit> {
    generated.save(local_dir).map_err(|e| Exit::error(1, e))?;
    if generated.target == Target::Project {
        cli::log!(
            "  {} {}",
            cli::check(),
            messages::start::config_created(generated.path())
        );
    }
    Ok(())
}

/// How the view ended.
enum End {
    /// A start option, with answers that passed [`check_answers`].
    Done(Box<Answers>),
    /// Esc or `abort`.
    Aborted,
    /// Ctrl+C or an interrupt signal.
    Interrupted,
}

/// Show the view until it ends (see [`run_wizard`] and
/// [`run_template_wizard`]) and return the new
/// config. Messages print after the terminal is restored.
async fn ask_config(input: &Input<'_>) -> Result<GeneratedConfig, Exit> {
    let mut form = Form::new(
        input.data_dir,
        input.packs,
        input.config.user_image(),
        input.templates,
        input.mode,
    );
    let mut screen = Screen::open()?;
    let end = Box::pin(run_view(input, &mut form, &mut screen)).await;
    let closed = screen.close();
    let end = end?;
    closed?;
    match end {
        End::Done(answers) => Ok(answers.config(input.host_cwd)),
        End::Aborted => {
            if let Some(e) = form.check_error() {
                return Err(Exit::config(e));
            }
            match input.mode {
                Mode::Start => cli::error!("{}", messages::start::ABORTED_NO_CONFIG),
                Mode::Template => cli::error!("{}", messages::start::ABORTED_NO_TEMPLATE),
            }
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
            Step::Abort => return Ok(End::Aborted),
            Step::Interrupt => return Ok(End::Interrupted),
            Step::Done(target) => {
                let answers = form.answers(target);
                // Leave raw mode and remove the view. Then the output of the
                // check (a vault passphrase prompt) shows as usual.
                screen.suspend()?;
                match check_answers(input, &answers).await {
                    Ok(()) => return Ok(End::Done(Box::new(answers))),
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
