//! The `airlock start` command.
//!
//! Reads the command-line options and starts the sandbox VM with the container
//! in it. The steps of the command are in the `start` module.

use clap::Args;

use crate::cli::{self, LogLevel};
use crate::config::config_values::Policy;
use crate::context::Context;
use crate::runtime::HostRuntime;
use crate::{config, packs, project, start};

/// CLI arguments for `airlock start`.
#[derive(Args, Debug)]
#[allow(clippy::struct_excessive_bools)] // independent flags
pub struct StartArgs {
    /// Log level
    #[arg(long, env = "AIRLOCK_LOG_LEVEL", default_value = "info")]
    pub log_level: LogLevel,
    /// Working directory in the container (default: the host cwd)
    #[arg(long)]
    pub sandbox_cwd: Option<String>,
    /// Run the container command in a login shell (reads /etc/profile, ~/.profile)
    #[arg(short = 'l', long)]
    pub login: bool,
    /// Show detailed output (mounts, network rules, sockets, port forwards)
    #[arg(short = 'v', long)]
    pub verbose: bool,
    /// Open the TUI monitor (sandbox and network tabs)
    #[arg(short = 'm', long)]
    pub monitor: bool,
    /// Use this network policy instead of `[network] policy`, for this run only
    #[arg(long, value_name = "POLICY")]
    pub network: Option<Policy>,
    /// Use the default answer for every sandbox question (re-create the sandbox)
    #[arg(short = 'y', long)]
    pub yes: bool,
}

impl StartArgs {
    /// Return the options for the sandbox and install steps.
    fn sandbox_options(&self) -> start::SandboxOptions {
        start::SandboxOptions {
            yes: self.yes,
            log_level: self.log_level,
            verbose: self.verbose,
        }
    }

    /// Return the config overrides from `--network`, for
    /// [`config::LayeredConfig::resolve`].
    fn config_overrides(&self) -> config::ConfigOverrides {
        config::ConfigOverrides {
            network_policy: self.network,
        }
    }
}

/// Entry point for `airlock start [--log-level <level>] [-- extra-args...]`.
/// Args:
///  - `args`: Parsed command-line arguments
///  - `extra_args`: Arguments after `--`, for the container command
///  - `context`: Shared CLI context (settings, vault and database)
///
/// Returns:
///   Exit code of the sandbox session, or error.
// The steps: system check, setup wizard (for a project without config),
// config, stored sandbox (image, disk, tool decisions) and tool installation
// (an install boot, see [`crate::packs::install::setup`]). Then the sandbox
// session runs ([`crate::start::run::run_sandbox`]).
pub async fn main(
    args: StartArgs,
    extra_args: Vec<String>,
    context: Context,
) -> anyhow::Result<i32> {
    cli::set_verbose(args.verbose);
    start::Exit::into_result(Box::pin(run(args, extra_args, context)).await)
}

async fn run(
    args: StartArgs,
    extra_args: Vec<String>,
    context: Context,
) -> Result<i32, start::Exit> {
    start::check_system_requirements();
    let host_cwd = start::resolve_host_cwd()?;
    start::init_logging(&host_cwd, args.log_level)?;

    let packs = packs::init().map_err(start::Exit::config)?;
    // A project without config gets the setup wizard. The generated file is
    // saved after the sandbox is stored.
    let config = Box::pin(start::wizard::load_or_generate_config(
        &host_cwd,
        &packs,
        &context.vault,
        context.settings.wizard_defaults.start,
    ))
    .await?;
    let resolved = config
        .resolve(&packs, &args.config_overrides())
        .await
        .map_err(start::Exit::config)?;
    // The terminal runtime. Bad `[monitor.keys]` values fail here.
    let runtime =
        HostRuntime::new(args.monitor, &context.settings).map_err(|e| start::Exit::error(2, e))?;

    let options = args.sandbox_options();
    let sandbox = Box::pin(start::sandbox::ensure_sandbox(
        &host_cwd,
        &packs,
        &resolved,
        &options,
        &context.vault,
    ))
    .await?;
    if let Some(generated) = config.generated_project() {
        start::wizard::save_config(generated)?;
    }
    let project = project::open(
        &sandbox.lock,
        resolved.values.clone(),
        args.sandbox_cwd.clone(),
        context,
    )?;
    Box::pin(start::install::install_tools(
        &project,
        &resolved,
        &sandbox.image,
        sandbox.installs,
        &options,
    ))
    .await?;
    Box::pin(start::run::run_sandbox(
        project,
        &sandbox.image,
        extra_args,
        &args,
        runtime,
    ))
    .await
}
