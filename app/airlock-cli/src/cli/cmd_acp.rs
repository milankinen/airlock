//! The `airlock acp` command.
//!
//! Runs an ACP agent for an editor in the sandbox of the project that the
//! editor opens.

use clap::Args;

use crate::cli::LogLevel;
use crate::context::Context;
use crate::{acp, cli, messages, oci};

/// CLI arguments for `airlock acp`.
#[derive(Args, Debug)]
pub struct AcpArgs {
    /// Log level of the proxy log file (acp.log in the airlock data dir)
    #[arg(long, env = "AIRLOCK_LOG_LEVEL", default_value = "info")]
    pub log_level: LogLevel,
    /// Give the agent all editor features, also host commands and host
    /// file access, and refuse no agent requests
    #[arg(long)]
    pub pass_client_capabilities: bool,
    /// In a git worktree, also look for the sandbox of the main repository
    #[arg(long)]
    pub follow_git_worktree: bool,
    /// Environment variables for the agent (KEY=VALUE)
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// Run the agent in a login shell (reads /etc/profile, ~/.profile)
    #[arg(short = 'l', long)]
    pub login: bool,
    /// Agent command. It can also come after `--`
    pub cmd: Option<String>,
    /// Arguments for the agent command
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
}

/// Entry point for `airlock acp [<cmd> [args...]] [-- <args...>]`.
/// Args:
///  - `args`: Parsed CLI arguments
///  - `extra_args`: Arguments after `--`. They come after `cmd` and `args`,
///    or give the command if `cmd` is not set
///  - `context`: Process context
///
/// Returns:
///   Exit code of the agent.
pub async fn main(
    args: AcpArgs,
    extra_args: Vec<String>,
    context: &Context,
) -> anyhow::Result<i32> {
    let AcpArgs {
        log_level,
        pass_client_capabilities,
        follow_git_worktree,
        env,
        login,
        cmd,
        args,
    } = args;
    let argv: Vec<String> = cmd.into_iter().chain(args).chain(extra_args).collect();
    let Some(name) = argv.first().cloned() else {
        cli::error!("{}", messages::acp::NO_COMMAND);
        std::process::exit(2);
    };
    let argv = if login {
        oci::apply_login_shell(argv)
    } else {
        argv
    };
    // Many proxies can write to the log at the same time, for example for
    // the agents of one editor. The process id tells their lines apart.
    // The log never goes to stdout, because stdout has the protocol.
    let prefix = format!("[{}] ", std::process::id());
    cli::logging::init_file(&context.data.root, acp::LOG_FILE, log_level, &prefix);
    tracing::info!("airlock version {}", cli::version_string(true));
    let options = acp::Options {
        argv,
        name,
        env,
        pass_client_capabilities,
        follow_git_worktree,
    };
    acp::run(context, options).await
}
