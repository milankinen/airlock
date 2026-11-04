//! The `airlock start` command.
//!
//! Reads the command-line options, finds the sandbox and starts the sandbox
//! process. The sandbox process prepares the sandbox on the same terminal.
//! Then the main command of the container runs in the sandbox.

use clap::Args;

use crate::cli::{self, LogLevel};
use crate::config::config_values::Policy;
use crate::context::Context;
use crate::messages;
use crate::runtime::HostRuntime;
use crate::sandbox::{self, Exit, ExitedBeforeReady, Interrupted, Run};

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
    /// Use the default answer for every sandbox question (re-create the sandbox, abort on a
    /// config change)
    #[arg(short = 'y', long)]
    pub yes: bool,
    /// Accept config changes since the last run without a question
    #[arg(long)]
    pub allow_configuration_changes: bool,
    /// Approve new or changed packs without a question
    #[arg(long)]
    pub auto_approve_packs: bool,
}

impl StartArgs {
    /// Return the start arguments for the sandbox process.
    /// Args:
    ///  - `extra_args`: Arguments after `--`, for the container command
    pub fn init_args(&self, extra_args: Vec<String>) -> sandbox::StartArgs {
        sandbox::StartArgs {
            args: extra_args,
            login: self.login,
            network: self.network,
            sandbox_cwd: self.sandbox_cwd.clone(),
            yes: self.yes,
            allow_config_changes: self.allow_configuration_changes,
            auto_approve_packs: self.auto_approve_packs,
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
///   Exit code of the main process or the TUI override, the exit code of
///   a sandbox process that stops before the sandbox is ready (for example
///   130 after a Ctrl+C or an abort, 2 for a config error), or error if the
///   sandbox runs or does not start.
// The steps: system check, the terminal runtime (bad `[monitor.keys]` fail
// early), the sandbox. Then the start waits for a sandbox that starts or
// stops, checks that the sandbox does not run, and starts the sandbox
// process. It does the questions, the config and the installs on the same
// terminal, and boots the sandbox. Then the main command runs in it. After
// the command, the browser notices show and the connection closes. If no
// other client uses the sandbox, the start waits until the sandbox
// stopped.
pub async fn main(
    args: StartArgs,
    extra_args: Vec<String>,
    context: Context,
) -> anyhow::Result<i32> {
    cli::set_verbose(args.verbose);
    Exit::into_result(Box::pin(run(args, extra_args, context)).await)
}

async fn run(args: StartArgs, extra_args: Vec<String>, context: Context) -> Result<i32, Exit> {
    sandbox::check_data_dir(&context.data)?;
    sandbox::check_system_requirements();
    let host_cwd = sandbox::resolve_host_cwd()?;
    sandbox::init_logging(args.log_level);
    // The terminal runtime. Bad `[monitor.keys]` values fail here.
    let runtime =
        HostRuntime::new(args.monitor, &context.settings).map_err(|e| Exit::error(2, e))?;
    let sandbox = sandbox::find_or_register(&context, &host_cwd).await?;
    // The log lines so far wait in memory. Now the log file is known.
    cli::logging::attach(sandbox.dir());
    let run = Run::Start {
        args: args.init_args(extra_args),
        log_level: args.log_level,
        verbose: args.verbose,
        monitor: runtime.approval_dialog(),
    };
    let mut process = match Box::pin(sandbox.run(&context, run)).await {
        Ok(process) => process,
        // The sandbox process gave the reason on the terminal. It holds the
        // run lock until its VM stopped, so the disk sync is complete.
        Err(e) if let Some(code) = ExitedBeforeReady::code_of(&e) => return Err(Exit::Code(code)),
        Err(e) if e.is::<Interrupted>() => return Err(Exit::ABORTED),
        Err(e) => return Err(e.into()),
    };
    let code = process.exec_main(runtime).await;
    for notice in process.notices().await {
        cli::log!(
            "{} {notice}",
            cli::yellow(messages::sandbox::BROWSER_NOTICE_LABEL)
        );
    }
    // The connection of this process holds the sandbox. The release tells
    // at once if the end of the hold stops the sandbox.
    if process.release().await {
        process.wait_stopped().await;
    }
    Ok(code?)
}
