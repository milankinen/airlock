//! The `airlock stop` command.
//!
//! Stops the sandbox of the project in the current directory, also when
//! clients use it. The connections of the clients close. Waits until the
//! sandbox stopped and its disk is synced. A sandbox that boots stops after
//! its boot.

use clap::Args;

use crate::context::Context;
use crate::sandbox::{self, FindOpts, Interrupted, STOP_WAIT_SECS, StopResult};
use crate::{cli, messages};

/// CLI arguments for `airlock stop`.
#[derive(Args, Debug)]
pub struct StopArgs {
    /// In a git worktree, also look for the sandbox of the main repository
    #[arg(long)]
    pub follow_git_worktree: bool,
}

/// Entry point for `airlock stop`.
/// Returns:
///   0 when the sandbox stopped or does not run, 1 if it does not stop in
///   time, 130 after a Ctrl+C. Error if the stop request fails.
// The lookup is the same as for `airlock exec`. Thus `stop` stops the
// sandbox that `exec` uses, also a sandbox that boots for its first start.
pub async fn main(args: &StopArgs, context: &Context) -> anyhow::Result<i32> {
    let opts = FindOpts {
        parents: true,
        follow_git_worktree: args.follow_git_worktree,
    };
    let Some(sandbox) = sandbox::find(context, &context.work, opts).await? else {
        cli::log!("{}", messages::cli::NOT_RUNNING);
        return Ok(0);
    };
    match sandbox.stop(context).await {
        Ok(StopResult::NotRunning) => {
            cli::log!("{}", messages::cli::NOT_RUNNING);
            Ok(0)
        }
        Ok(StopResult::Stopped) => {
            let project = sandbox.path().display();
            cli::log!("{} {}", cli::check(), messages::cli::stopped(project));
            Ok(0)
        }
        Ok(StopResult::Timeout) => {
            cli::error!("{}", messages::cli::stop_timeout(STOP_WAIT_SECS));
            Ok(1)
        }
        Err(e) if e.is::<Interrupted>() => Ok(130), // 128 + SIGINT
        Err(e) => Err(e),
    }
}
