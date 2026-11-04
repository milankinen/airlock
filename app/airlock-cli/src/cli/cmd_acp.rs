//! The `airlock acp` command.
//!
//! Runs an ACP agent for an editor in the sandbox of the project that the
//! editor opens.

use clap::Args;

use crate::cli::LogLevel;
use crate::context::Context;
use crate::{acp, cli, messages, oci};

/// CLI arguments for `airlock acp`.
#[allow(clippy::struct_excessive_bools)] // independent flags
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
    /// Give the agent the MCP servers of the editor. They run on the host,
    /// and their tools act on the host
    #[arg(long)]
    pub proxy_mcp_servers: bool,
    /// Agent features that airlock offers to the editor, comma separated,
    /// for example 'loadSession,steering'. Replaces the preset of the
    /// agent. An empty value offers no features
    #[arg(long, value_name = "NAMES")]
    pub set_agent_capabilities: Option<String>,
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
        proxy_mcp_servers,
        set_agent_capabilities,
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
    let (argv, agent_capabilities) = match resolve(argv, login, set_agent_capabilities.as_deref()) {
        Ok(resolved) => resolved,
        Err(message) => {
            cli::error!("{message}");
            std::process::exit(2);
        }
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
        proxy_mcp_servers,
        log_level,
        agent_capabilities,
    };
    acp::run(context, options).await
}

/// Get the agent command for the sandbox and the agent feature set.
/// Args:
///  - `argv`: Agent command and its arguments as the user gave them
///  - `login`: Run the agent in a login shell
///  - `set`: Value of `--set-agent-capabilities`
///
/// Returns:
///   The command for the sandbox and the feature names, or the error
///   message for unknown feature names.
// The preset follows the agent command before the login shell wrap. The
// wrap starts the command with the shell.
fn resolve(
    argv: Vec<String>,
    login: bool,
    set: Option<&str>,
) -> Result<(Vec<String>, Vec<String>), String> {
    let features = match set {
        Some(set) => acp::capabilities::parse(set).map_err(|unknown| {
            messages::acp::unknown_agent_capabilities(
                unknown.join(", "),
                acp::capabilities::valid_names(),
            )
        })?,
        None => acp::capabilities::preset(&argv),
    };
    let argv = if login {
        oci::apply_login_shell(argv)
    } else {
        argv
    };
    Ok((argv, features))
}

#[cfg(test)]
mod tests {
    //! Tests for the agent feature set of `airlock acp`.

    use super::*;

    /// Test that the login shell wrap keeps the preset of the agent. The
    /// wrap puts a shell in front of the agent command.
    ///   1. Resolve the Claude agent and Copilot with `--acp` with and
    ///      without the login shell
    ///   2. Check that the command has the shell and the presets are the
    ///      same
    ///   3. Check that a set replaces the preset, and that an unknown name
    ///      is refused
    #[test]
    fn login_shell_keeps_agent_preset() {
        let argv = |a: &[&str]| a.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
        for cmd in [argv(&["claude-agent-acp"]), argv(&["copilot", "--acp"])] {
            let (plain_argv, plain) = resolve(cmd.clone(), false, None).unwrap();
            let (login_argv, login) = resolve(cmd.clone(), true, None).unwrap();
            assert_eq!(plain_argv, cmd);
            assert_ne!(login_argv, plain_argv);
            assert_eq!(login, plain);
            assert_eq!(plain, acp::capabilities::preset(&cmd));
        }
        let (_, set) = resolve(argv(&["claude-agent-acp"]), true, Some("steering")).unwrap();
        assert_eq!(set, ["steering"]);
        let error = resolve(argv(&["x"]), false, Some("nope")).unwrap_err();
        assert!(error.contains("nope"), "{error}");
    }
}
