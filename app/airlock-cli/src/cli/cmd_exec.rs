//! The `airlock exec` command.
//!
//! Runs a process in the container of a sandbox. Starts the sandbox with
//! the config of the last `airlock start` if it does not run. The sandbox
//! process does the work. The start shows no progress, only the vault
//! question and errors. The start does not apply changes of the project
//! config: a notice of the sandbox tells about them. After the connect,
//! the command prints all notices of the sandbox as warnings, also when
//! the sandbox already ran.

use clap::Args;

use crate::context::Context;
use crate::runtime::RawTerminalRuntime;
use crate::sandbox::{self, ExecRequest, ExitedBeforeReady, FindOpts, Interrupted, Run};
use crate::{messages, oci};

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
///   Exit code of the process, 130 after a Ctrl+C during the sandbox
///   start, or error if the sandbox cannot start.
// The command, the guest dir of the caller's cwd and the `-e KEY=VAL`
// overrides go to the sandbox process. The sandbox process has the
// resolved sandbox environment (image env and `airlock.toml` env). It
// merges the overrides into that env and tells the supervisor to start the
// process. Thus `exec` never loads the project and never opens the vault.
// It uses only the sandbox registry of the context and the run record of
// the sandbox.
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

    let opts = FindOpts {
        parents: true,
        follow_git_worktree,
    };
    let Some(sandbox) = sandbox::find(context, &context.work, opts).await? else {
        anyhow::bail!(messages::cli::never_started(&context.work));
    };
    // The connection holds the sandbox until this command ends.
    let mut process = match sandbox.run(context, Run::Exec).await {
        Ok(process) => process,
        Err(e) if e.is::<Interrupted>() => return Ok(130), // 128 + SIGINT
        // The sandbox process gave the reason on the terminal.
        Err(e) if let Some(code) = ExitedBeforeReady::code_of(&e) => return Ok(code),
        Err(e) => return Err(e),
    };
    // Show the notices of the sandbox before the process takes the
    // terminal.
    process.show_notices(0).await;
    // The guest dir of the current dir is the default guest cwd. Get it
    // after the connect: a start that ran at the same time can change the
    // guest cwd.
    let cwd = match cwd {
        Some(cwd) => cwd,
        None => process.guest_dir(&context.work)?,
    };
    let request = ExecRequest {
        argv: &argv,
        cwd: &cwd,
        env: &env,
    };
    process.exec(RawTerminalRuntime::new(), &request).await
}
