//! The start work of `airlock start` in the sandbox process.
//!
//! Runs on the terminal of the client, under the held sandbox locks. Runs
//! the setup wizard for a project without config, asks about new or changed
//! packs, fills a new sandbox, asks about config changes, prepares the
//! stored sandbox, installs the tools and opens the project. The result is
//! ready for the boot.

use super::{Exit, SandboxOptions, ensure, install, location, wizard};
use crate::cli::{self, LogLevel};
use crate::config::{self, ConfigOverrides};
use crate::context::Context;
use crate::oci::OciImage;
use crate::sandbox::first_start::FirstStart;
use crate::sandbox::lock::SandboxLock;
use crate::sandbox::process::init::StartArgs;
use crate::sandbox::{self, Project};
use crate::{messages, packs, templates};

/// A prepared sandbox, ready for the boot.
pub struct Prepared {
    /// The opened project with the config of the run.
    pub project: Project,
    /// The prepared container image.
    pub image: OciImage,
    /// The main command of the session.
    pub main_argv: Vec<String>,
}

/// Do the start work of `airlock start` for the sandbox that `lock` holds.
/// Questions go to the terminal, if there is one.
/// Args:
///  - `context`: Process context
///  - `lock`: The held sandbox locks
///  - `first`: The first start state of the sandbox. A move of user files
///    keeps the sandbox
///  - `args`: The start arguments
///  - `log_level`: Log level for the install boot (`--log-level`)
///
/// Returns:
///   The prepared sandbox, or the exit for an error or an aborted question.
pub async fn prepare(
    context: Context,
    lock: &SandboxLock,
    first: &mut FirstStart,
    args: &StartArgs,
    log_level: LogLevel,
) -> Result<Prepared, Exit> {
    let options = SandboxOptions {
        yes: args.yes,
        can_prompt: cli::prompt::can_prompt(),
        log_level,
        verbose: cli::is_verbose(),
        allow_config_changes: args.allow_config_changes,
        // `--network` applies to this run only.
        overrides: ConfigOverrides {
            network_policy: args.network,
        },
        auto_approve_packs: args.auto_approve_packs,
    };
    let host_cwd = lock.host_cwd();
    let packs = packs::init(&context.settings.packs, &context.home).map_err(Exit::config)?;
    // A new sandbox gets the local project config from `.airlock` only after
    // the config resolve. Thus a config error or an abort leaves `.airlock`
    // as it is. Until then, the config loads from `.airlock`. A sandbox of
    // an older version in the project counts as a sandbox.
    let sandbox_config_dir =
        sandbox::lookup::local_config_dir(lock.dir()).map_err(anyhow::Error::from)?;
    let load_dir = if first.is_new() {
        host_cwd.join(".airlock").map_err(anyhow::Error::from)?
    } else {
        sandbox_config_dir.clone()
    };
    let has_sandbox = sandbox::lookup::has_legacy_sandbox(host_cwd)
        || sandbox::lookup::dir_has_content(lock.dir());
    // A project without config gets the setup wizard. The generated file is
    // saved after the sandbox is stored.
    let config = Box::pin(wizard::load_or_generate_config(
        host_cwd,
        &context.data,
        &load_dir,
        has_sandbox,
        &packs,
        &context.vault,
        &templates::list(&context.data),
    ))
    .await?;
    if config.has_user_files() {
        cli::log!(
            "{} {}",
            cli::yellow("!"),
            messages::start::USER_CONFIG_FILES_DEPRECATED
        );
    }
    // The first resolve asks about new or changed packs, before any pack
    // `config.lua` runs. The later resolves do not ask.
    let resolved = config
        .resolve(&config::ResolveInput {
            data_dir: &context.data,
            packs: &packs,
            overrides: &options.overrides,
            project_root: Some(host_cwd),
            approval: options.approval_mode(),
        })
        .await
        .map_err(Exit::resolve)?;
    // A wizard config can name a template for the new sandbox.
    let template = config
        .generated_project()
        .and_then(|g| g.template.as_deref())
        .map(|name| templates::find(&context.data, name))
        .transpose()
        .map_err(|e| Exit::error(1, e))?;
    location::fill_new_sandbox(&context, lock, first, options.can_prompt, options.yes)?;
    // The sandbox step can select the config of the last run.
    let ensured = Box::pin(ensure::ensure_sandbox(
        &context,
        lock,
        &packs,
        &config,
        resolved,
        &options,
        template.as_ref(),
    ))
    .await?;
    if let Some(generated) = config.generated_project() {
        wizard::save_config(generated, &sandbox_config_dir)?;
    }
    let resolved = &ensured.resolved;
    // The open checks `[env]` and records the guest cwd for `airlock exec`.
    let project = sandbox::open(
        lock,
        resolved.values.clone(),
        args.sandbox_cwd.clone(),
        context,
    )?;
    // The install boot stops its VM before it returns. Thus only the boot of
    // the sandbox runs a VM next.
    Box::pin(install::install_tools(
        &project,
        resolved,
        &ensured.image,
        ensured.installs,
        &options,
    ))
    .await?;
    let main_argv =
        crate::sandbox::process::vm::main_argv(args.args.clone(), args.login, &ensured.image);
    Ok(Prepared {
        project,
        image: ensured.image,
        main_argv,
    })
}
