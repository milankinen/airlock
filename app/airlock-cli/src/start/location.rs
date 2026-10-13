//! The sandbox location step of `airlock start`.
//!
//! Finds or makes the sandbox of the project and takes its lock. A new
//! sandbox goes to the location that the user settings select. For a sandbox
//! in the project directory, the step can offer to move it into the airlock
//! data directory, where the sandbox guest cannot see it.

use std::path::Path;

use super::Exit;
use crate::cli::prompt::PromptError;
use crate::cli::prompt::choose::{Choice, Choose};
use crate::context::Context;
use crate::project::{self, SandboxLock};
use crate::sandboxes::{self, Found, KEEP_IN_PROJECT, Location, MigrateError, registry};
use crate::settings::SandboxType;
use crate::util::{PinnedDir, vfs};
use crate::{cli, messages};

/// The answer to the move question.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Move {
    /// Move the sandbox into the data directory.
    Yes,
    /// Keep the sandbox in the project and do not ask again.
    No,
    /// Nobody can answer now. Keep the sandbox and ask on a later start.
    Later,
}

/// Find or make the sandbox of the project `host_cwd` and take its lock.
/// Args:
///  - `context`: Process context (settings, registry, data directory)
///  - `host_cwd`: Canonical project directory
///  - `can_prompt`: A terminal is available for questions
///  - `yes`: `--yes`: move a project sandbox without a question
///
/// Returns:
///   The held lock, or the exit for an error or an aborted question.
pub async fn lock_sandbox(
    context: &Context,
    host_cwd: &Path,
    can_prompt: bool,
    yes: bool,
) -> Result<SandboxLock, Exit> {
    let found = sandboxes::resolve_sandbox(context, host_cwd, false, false).await?;
    let lock = match found {
        Some(Found {
            location: Location::DataDir { id },
            ..
        }) => lock_box(context, host_cwd, &id).await?,
        Some(found) if wants_move(context, &found) => match ask_move(&found, can_prompt, yes)? {
            Move::Yes => move_to_data_dir(context, host_cwd).await?,
            Move::No => {
                let lock = SandboxLock::acquire(host_cwd)?;
                lock.dir()
                    .to_path()
                    .and_then(PinnedDir::pin)
                    .and_then(|dir| dir.write_atomic(KEEP_IN_PROJECT, b"", 0o600))
                    .map_err(anyhow::Error::from)?;
                lock
            }
            Move::Later => SandboxLock::acquire(host_cwd)?,
        },
        Some(_) => SandboxLock::acquire(host_cwd)?,
        None => match context.settings.security.sandbox_type {
            SandboxType::ProjectOwned => SandboxLock::acquire(host_cwd)?,
            SandboxType::Managed => {
                // A local project config in `.airlock` goes with the new
                // sandbox. Check before anything is made.
                sandboxes::check_local_config(context, host_cwd).map_err(|e| Exit::error(1, e))?;
                let boxes = sandboxes::boxes_dir(context).map_err(anyhow::Error::from)?;
                let id = registry::find_or_register(&context.db, &boxes, host_cwd).await?;
                let lock = lock_box(context, host_cwd, &id).await?;
                let dir = lock.dir().to_path().map_err(anyhow::Error::from)?;
                sandboxes::adopt_local_config(context, host_cwd, dir)
                    .map_err(|e| Exit::error(1, e))?;
                lock
            }
        },
    };
    Ok(lock)
}

/// Take the lock of the sandbox `id` in the data directory.
// `airlock sandboxes rm` can remove the sandbox between the registry lookup
// and the lock. The lock then makes a new empty directory. Check the
// registry again under the lock, and remove that directory.
async fn lock_box(context: &Context, host_cwd: &Path, id: &str) -> Result<SandboxLock, Exit> {
    let boxes = sandboxes::boxes_dir(context).map_err(anyhow::Error::from)?;
    let lock = SandboxLock::acquire_box(host_cwd, &boxes, id)?;
    if registry::get(&context.db, id).await?.as_deref() != Some(host_cwd) {
        let dir = lock.dir().clone();
        drop(lock);
        let _ = vfs::remove_dir_all(&dir);
        return Err(Exit::error(
            1,
            messages::start::SANDBOX_REMOVED_DURING_START,
        ));
    }
    Ok(lock)
}

/// Check if the move question applies to the project sandbox `found`: the
/// settings select the data directory, the user did not choose to keep the
/// sandbox in the project, and no airlock process runs the sandbox.
fn wants_move(context: &Context, found: &Found) -> bool {
    if context.settings.security.sandbox_type != SandboxType::Managed
        || project::is_running(&found.dir)
    {
        return false;
    }
    // A symlinked sandbox directory gets no question. The lock reports the
    // problem.
    if !vfs::symlink_metadata(&found.dir).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    found
        .dir
        .join(KEEP_IN_PROJECT)
        .and_then(|p| vfs::symlink_metadata(&p))
        .is_err()
}

/// Ask if the project sandbox `found` moves into the data directory. The
/// answers are "Yes, migrate" (default), "No, keep current" and "Abort".
/// Abort and Esc end the run.
fn ask_move(found: &Found, can_prompt: bool, yes: bool) -> Result<Move, Exit> {
    if yes {
        return Ok(Move::Yes);
    }
    if !can_prompt {
        cli::log!(
            "{} {}",
            cli::yellow("!"),
            messages::start::sandbox_data_visible(&found.dir)
        );
        return Ok(Move::Later);
    }
    let choices = [
        messages::start::MOVE_CHOICE_YES,
        messages::start::MOVE_CHOICE_NO,
        messages::cli::ABORT_CHOICE,
    ]
    .map(|label| Choice { label });
    let answer = Choose {
        title: messages::start::MOVE_TITLE,
        notes: &[messages::start::MOVE_NOTE],
        choices: &choices,
        default: 0,
        report: true,
    }
    .ask();
    match answer {
        Ok(Some(0)) => Ok(Move::Yes),
        Ok(Some(1)) => Ok(Move::No),
        // Abort and Esc end the run.
        Ok(_) => Err(Exit::aborted()),
        Err(PromptError::NotInteractive) => Ok(Move::Later),
        Err(e) => Err(e.into()),
    }
}

/// Move the project sandbox into the data directory and take its lock.
async fn move_to_data_dir(context: &Context, host_cwd: &Path) -> Result<SandboxLock, Exit> {
    let id = match sandboxes::migrate(context, host_cwd).await {
        Ok(id) => id,
        // The same failure as a lock that a different instance holds.
        Err(MigrateError::Running) => {
            return Err(anyhow::anyhow!(messages::project::SANDBOX_IN_USE).into());
        }
        Err(e) => return Err(Exit::error(1, e)),
    };
    let lock = lock_box(context, host_cwd, &id).await?;
    cli::log!(
        "  {} {}",
        cli::check(),
        messages::start::sandbox_moved(lock.dir())
    );
    Ok(lock)
}
