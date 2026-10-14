//! The `airlock exec` command.
//!
//! Runs a process in the container of a running sandbox. The `airlock start`
//! process of that sandbox does the work.

use clap::Args;

use crate::cli::sandbox_client::SandboxClient;
use crate::context::Context;
use crate::runtime::{self, RawTerminalRuntime};
use crate::{messages, oci, rpc, sandbox};

/// CLI arguments for `airlock exec`.
#[derive(Args, Debug)]
pub struct ExecArgs {
    /// Command to run
    pub cmd: String,
    /// Arguments for the command
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub args: Vec<String>,
    /// Working directory inside the container
    #[arg(short = 'w', long)]
    pub cwd: Option<String>,
    /// Environment variables (KEY=VALUE)
    #[arg(short = 'e', long = "env")]
    pub env: Vec<String>,
    /// Run the command in a login shell (reads /etc/profile, ~/.profile)
    #[arg(short = 'l', long)]
    pub login: bool,
    /// In a git worktree, also look for the sandbox of the main repository
    #[arg(long)]
    pub follow_git_worktree: bool,
}

/// Entry point for `airlock exec <cmd> [args...]`.
/// Returns:
///   Exit code of the process, or error if no sandbox runs.
// The command, the caller's cwd and the `-e KEY=VAL` overrides go to the
// `airlock start` process. That process has the resolved sandbox environment
// (image env and `airlock.toml` env). It merges the overrides into that env
// and tells the supervisor to start the process. Thus `exec` never loads the
// project or the vault. It uses only the sandbox registry of the context.
pub async fn main(args: ExecArgs, context: &Context) -> anyhow::Result<i32> {
    let ExecArgs {
        cmd,
        args,
        cwd,
        env,
        login,
        follow_git_worktree,
    } = args;
    let argv: Vec<String> = std::iter::once(cmd).chain(args).collect();
    let argv = if login {
        oci::apply_login_shell(argv)
    } else {
        argv
    };
    let (cmd, args) = argv.split_first().expect("argv holds the command");

    let host_cwd = std::env::current_dir()
        .map_err(|e| anyhow::anyhow!(messages::cli::exec_get_cwd_failed(e)))?;
    let client = SandboxClient::connect(context, &host_cwd, follow_git_worktree).await?;
    let cli_service = client.service;
    tokio::task::spawn_local(client.rpc_system);

    let raw = RawTerminalRuntime::new();
    let stdin = raw.stdin()?;
    let pty_size = stdin.pty_size();

    let mut req = cli_service.exec_request();
    req.get().set_stdin(capnp_rpc::new_client(stdin));
    rpc::set_pty(req.get().init_pty(), pty_size);
    req.get().set_cmd(cmd);
    let mut args_b = req.get().init_args(args.len() as u32);
    for (i, a) in args.iter().enumerate() {
        args_b.set(i as u32, a.as_str());
    }
    let cwd = cwd.unwrap_or_else(|| host_cwd.to_string_lossy().into_owned());
    req.get().set_cwd(&cwd);

    let mut env_b = req.get().init_env(env.len() as u32);
    for (i, e) in env.iter().enumerate() {
        env_b.set(i as u32, e.as_str());
    }

    let response = req.send().promise.await?;
    let proc = rpc::Process::new(response.get()?.get_proc()?);

    let mut terminal = raw.into_terminal();
    tokio::task::spawn_local(runtime::forward_signals(runtime::signals()?, proc.clone()));
    Ok(sandbox::io::drive(&proc, &mut terminal).await)
}
