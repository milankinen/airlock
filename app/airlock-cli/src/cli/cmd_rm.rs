//! The `airlock rm` command.
//!
//! Deletes the sandbox of the project in the current directory from the
//! airlock data directory, with the local project config.

use clap::Args;

use crate::cli::prompt::choose;
use crate::context::Context;
use crate::sandbox::{self, FindOpts, Sandbox};
use crate::util::verified_path::VerifiedPath;
use crate::{cli, messages, oci};

/// CLI arguments for `airlock rm`.
#[derive(Args, Debug)]
pub struct RmArgs {
    /// Do not ask for confirmation
    #[arg(short = 'f', long)]
    pub force: bool,
}

/// Remove the sandbox of the project after confirmation (unless `--force`).
///
/// The sandbox goes with its registry entry and its local project config.
/// The project `.airlock/` directory stays.
/// Returns:
///   Process exit code: 0 on success or abort, 1 on error.
// The config is not loaded. `rm` needs only the sandbox paths, and a broken
// config is a common reason to start again.
//
// The sandbox lock stays held until the removal is complete. Thus a parallel
// `airlock start` cannot take the sandbox during the removal.
pub async fn main(args: &RmArgs, context: &Context) -> i32 {
    run(args, context, &context.work).await
}

/// Body of [`main`], with the project directory as an argument.
// The argument lets tests run it without a change to the current directory
// of the process.
pub(super) async fn run(args: &RmArgs, context: &Context, host_cwd: &VerifiedPath) -> i32 {
    match sandbox::find(context, host_cwd, FindOpts::default()).await {
        Ok(Some(sandbox)) => rm_box(args, context, &sandbox).await,
        Ok(None) => {
            cli::log!("{}", messages::cli::NO_SANDBOX_TO_REMOVE);
            0
        }
        Err(e) => {
            cli::error!("{}", messages::cli::sandbox_lookup_failed(format!("{e:#}")));
            1
        }
    }
}

/// Remove the sandbox `sandbox` from the data directory.
/// Returns:
///   Process exit code.
async fn rm_box(args: &RmArgs, context: &Context, sandbox: &Sandbox) -> i32 {
    // Fail before the question. The removal checks again under the lock.
    if sandbox.is_running() {
        cli::error!("{}", messages::sandbox::SANDBOX_RUNNING_STOP_FIRST);
        return 1;
    }
    if !confirm(args) {
        cli::error!("{}", messages::cli::ABORTED);
        return 0;
    }
    if let Err(e) = sandbox.remove(context).await {
        cli::error!(
            "{}",
            messages::cli::failed_to_remove_sandbox(format!("{e:#}"))
        );
        return 1;
    }
    oci::gc_sweep(&context.data);
    cli::log!("{}", messages::cli::SANDBOX_REMOVED);
    0
}
/// Ask to confirm the removal of the sandbox. With `--force`, do not ask.
/// Returns:
///   True if confirmed. Abort, Esc, no terminal or a failed prompt is "no".
fn confirm(args: &RmArgs) -> bool {
    if args.force {
        return true;
    }
    matches!(
        choose::confirm(
            messages::cli::REMOVE_SANDBOX_TITLE,
            &[],
            messages::cli::REMOVE_SANDBOX_YES
        ),
        Ok(true)
    )
}
