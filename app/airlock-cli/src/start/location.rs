//! The sandbox location step of `airlock start`.
//!
//! Finds or makes the sandbox of the project in the airlock data directory
//! and takes its lock. A sandbox that an older version kept in the project
//! directory moves into the data directory first, after a question.

use super::Exit;
use crate::cli::prompt::PromptError;
use crate::cli::prompt::choose::{Choice, Choose};
use crate::context::Context;
use crate::project::SandboxLock;
use crate::sandboxes::{self, MigrateError, registry};
use crate::util::VerifiedPath;
use crate::{cli, messages};

/// Find or make the sandbox of the project `host_cwd` and take its lock.
/// Args:
///  - `context`: Process context (settings, registry, data directory)
///  - `host_cwd`: Canonical project directory
///  - `can_prompt`: A terminal is available for questions
///  - `yes`: `--yes`: move an old project sandbox without a question
///
/// Returns:
///   The held lock, or the exit for an error or an aborted question.
pub async fn lock_sandbox(
    context: &Context,
    host_cwd: &VerifiedPath,
    can_prompt: bool,
    yes: bool,
) -> Result<SandboxLock, Exit> {
    // A registered sandbox wins. A stale `.airlock/sandbox` next to it
    // stays as it is.
    if let Some(found) = sandboxes::resolve_sandbox(context, host_cwd, false, false).await? {
        return lock_box(context, host_cwd, &found.id).await;
    }
    if sandboxes::has_legacy_sandbox(host_cwd) {
        ask_import(can_prompt, yes)?;
        return move_to_data_dir(context, host_cwd).await;
    }
    // A local project config in `.airlock` goes with the new sandbox. Check
    // before anything is made.
    sandboxes::check_local_config(host_cwd).map_err(|e| Exit::error(1, e))?;
    let boxes = &context.data.sandboxes;
    let id = registry::find_or_register(&context.db, boxes, host_cwd).await?;
    let lock = lock_box(context, host_cwd, &id).await?;
    sandboxes::adopt_local_config(host_cwd, lock.dir()).map_err(|e| Exit::error(1, e))?;
    Ok(lock)
}

/// Take the lock of the sandbox `id` in the data directory.
async fn lock_box(
    context: &Context,
    host_cwd: &VerifiedPath,
    id: &str,
) -> Result<SandboxLock, Exit> {
    sandboxes::lock_box(context, host_cwd, id)
        .await?
        .ok_or_else(|| Exit::error(1, messages::start::SANDBOX_REMOVED_DURING_START))
}

/// Ask before an old project sandbox moves into the data directory. The
/// choices are "Continue" (default) and "Abort".
/// Returns:
///   `Ok` to continue. The exit for Abort or Esc, or an error if no
///   terminal is available and `yes` is not set.
fn ask_import(can_prompt: bool, yes: bool) -> Result<(), Exit> {
    if yes {
        return Ok(());
    }
    let no_prompt = || Exit::error(1, messages::start::LEGACY_SANDBOX_NO_PROMPT);
    if !can_prompt {
        return Err(no_prompt());
    }
    let choices = [
        messages::start::IMPORT_CONTINUE,
        messages::cli::ABORT_CHOICE,
    ]
    .map(|label| Choice { label });
    let answer = Choose {
        title: messages::start::IMPORT_TITLE,
        notes: &[],
        choices: &choices,
        default: 0,
        report: true,
    }
    .ask();
    match answer {
        Ok(Some(0)) => Ok(()),
        // Abort and Esc end the run.
        Ok(_) => Err(Exit::aborted()),
        Err(PromptError::NotInteractive) => Err(no_prompt()),
        Err(e) => Err(e.into()),
    }
}

/// Import the project sandbox into the data directory and take its lock.
async fn move_to_data_dir(context: &Context, host_cwd: &VerifiedPath) -> Result<SandboxLock, Exit> {
    let lock = match sandboxes::import_legacy(context, host_cwd).await {
        Ok(lock) => lock,
        // The same failure as a lock that a different instance holds.
        Err(MigrateError::Running) => {
            return Err(anyhow::anyhow!(messages::project::SANDBOX_IN_USE).into());
        }
        Err(e) => return Err(Exit::error(1, e)),
    };
    cli::log!(
        "  {} {}",
        cli::check(),
        messages::start::sandbox_moved(lock.dir())
    );
    Ok(lock)
}
