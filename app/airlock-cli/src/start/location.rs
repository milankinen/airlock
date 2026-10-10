//! The sandbox location step of `airlock start`.
//!
//! Finds or makes the sandbox of the project and takes its lock. A new
//! sandbox goes to the location that the user settings select. For a sandbox
//! in the project directory, the step can offer to move it into the airlock
//! data directory, where the sandbox guest cannot see it.

use std::path::Path;

use super::Exit;
use crate::cli;
use crate::cli::prompt::PromptError;
use crate::cli::prompt::choose::{Choice, Choose};
use crate::cli::prompt::style::Tone;
use crate::context::Context;
use crate::project::{self, SandboxLock};
use crate::sandboxes::{self, Found, KEEP_IN_PROJECT, Location, MigrateError, registry};
use crate::settings::SandboxType;
use crate::util::PinnedDir;

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
///   The held lock, or the exit for an error or a cancelled question.
pub async fn lock_sandbox(
    context: &Context,
    host_cwd: &Path,
    can_prompt: bool,
    yes: bool,
) -> Result<SandboxLock, Exit> {
    let found = sandboxes::resolve_sandbox(context, host_cwd, false).await?;
    let lock = match found {
        Some(Found {
            location: Location::DataDir { id },
            ..
        }) => lock_box(context, host_cwd, &id).await?,
        Some(found) if wants_move(context, &found) => match ask_move(&found, can_prompt, yes)? {
            Move::Yes => move_to_data_dir(context, host_cwd).await?,
            Move::No => {
                let lock = SandboxLock::acquire(host_cwd)?;
                PinnedDir::pin(lock.dir())
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
                sandboxes::check_local_config(host_cwd).map_err(|e| Exit::error(1, e))?;
                let boxes = context.boxes_dir();
                let id = registry::find_or_register(&context.db, &boxes, host_cwd).await?;
                let lock = lock_box(context, host_cwd, &id).await?;
                sandboxes::adopt_local_config(host_cwd, lock.dir())
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
    let lock = SandboxLock::acquire_box(host_cwd, &context.boxes_dir(), id)?;
    if registry::get(&context.db, id).await?.as_deref() != Some(host_cwd) {
        let dir = lock.dir().to_path_buf();
        drop(lock);
        let _ = std::fs::remove_dir_all(&dir);
        return Err(Exit::error(
            1,
            "the sandbox was removed while airlock started it; run `airlock start` again",
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
    if !std::fs::symlink_metadata(&found.dir).is_ok_and(|m| m.is_dir()) {
        return false;
    }
    std::fs::symlink_metadata(found.dir.join(KEEP_IN_PROJECT)).is_err()
}

/// Ask if the project sandbox `found` moves into the data directory. The
/// answers are "Yes, migrate" (default), "No, keep current" and "Cancel".
/// Cancel and Esc end the run.
fn ask_move(found: &Found, can_prompt: bool, yes: bool) -> Result<Move, Exit> {
    if yes {
        return Ok(Move::Yes);
    }
    if !can_prompt {
        cli::log!(
            "{} the sandbox data is in {}, where the sandbox can see it; run `airlock start` \
             in a terminal to move it",
            cli::yellow("!"),
            found.dir.display()
        );
        return Ok(Move::Later);
    }
    let choices = ["Yes, migrate", "No, keep current", "Cancel"].map(|label| Choice {
        label,
        tone: Tone::Plain,
    });
    let answer = Choose {
        title: "Migrate sandbox to secure directory?",
        notes: &["See airlock v2026.10.2 release notes for details"],
        choices: &choices,
        default: 0,
        report: true,
    }
    .ask();
    match answer {
        Ok(Some(0)) => Ok(Move::Yes),
        Ok(Some(1)) => Ok(Move::No),
        // Cancel and Esc end the run.
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
            return Err(anyhow::anyhow!("another airlock instance is using this sandbox").into());
        }
        Err(e) => return Err(Exit::error(1, e)),
    };
    let lock = lock_box(context, host_cwd, &id).await?;
    cli::log!(
        "  {} sandbox moved to {}",
        cli::check(),
        lock.dir().display()
    );
    Ok(lock)
}
