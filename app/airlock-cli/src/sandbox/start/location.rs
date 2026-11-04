//! The sandbox location step of `airlock start`.
//!
//! Finds or registers the sandbox of the project, with no questions. Then
//! fills a new sandbox: a sandbox that an older version kept in the project
//! moves into it after a question, or the local project config moves into
//! it.

use super::Exit;
use crate::cli::prompt::PromptError;
use crate::cli::prompt::choose::{Choice, Choose};
use crate::context::Context;
use crate::sandbox::first_start::FirstStart;
use crate::sandbox::lock::SandboxLock;
use crate::sandbox::migrate::{self, MigrateError};
use crate::sandbox::{self, Sandbox, registry};
use crate::util::VerifiedPath;
use crate::{cli, messages};

/// Find or register the sandbox of the project `host_cwd`, and make its
/// directory. Asks no questions and takes no lock. For `airlock start`.
/// Args:
///  - `context`: Process context (settings, registry, data directory)
///  - `host_cwd`: Canonical project directory
///
/// Returns:
///   The sandbox, or the exit for an error.
pub async fn find_or_register(context: &Context, host_cwd: &VerifiedPath) -> Result<Sandbox, Exit> {
    let candidates = sandbox::lookup::candidates(context, host_cwd, false, false).await?;
    let id = if let Some(found) = candidates.into_iter().next() {
        found.id
    } else {
        // A local project config in `.airlock` goes with the new sandbox.
        // Check before anything is made.
        sandbox::migrate::check_local_config(host_cwd).map_err(|e| Exit::error(1, e))?;
        let boxes = &context.data.sandboxes;
        registry::find_or_register(&context.db, boxes, host_cwd).await?
    };
    let dir = sandbox::lock::create_box_dir(&context.data.sandboxes, &id)?;
    Ok(Sandbox::new(id, host_cwd.clone(), dir))
}

/// Check that `sandbox` is still in the registry. Call it with the startup
/// lock held.
/// Returns:
///   Error if `airlock sandboxes rm` removed the sandbox after the lookup.
///   The directory of the removed sandbox goes then too.
// The removal needs free locks. A removal between the lookup and the
// lock, or the directory step, can leave an empty directory. It goes here.
pub async fn check_registered(context: &Context, sandbox: &Sandbox) -> anyhow::Result<()> {
    let registered = registry::get(&context.db, sandbox.id()).await?;
    // The registry has the canonical path. A project path that changed
    // since the lookup is not the registered project.
    let project = sandbox.project()?.to_entry_path()?;
    if registered.as_deref() != Some(project.as_os_str()) {
        let _ = sandbox.dir().remove_dir_all();
        anyhow::bail!(messages::start::SANDBOX_REMOVED_DURING_START);
    }
    Ok(())
}

/// Fill the new sandbox that `lock` holds. A sandbox of an older version in
/// the project directory moves into it after a question. Else the local
/// project config in `.airlock` moves into it. A sandbox that is not new
/// stays as it is, also with a stale `.airlock` next to it.
/// Args:
///  - `context`: Process context, for the image cache
///  - `lock`: The held sandbox locks
///  - `first`: The first start state of the sandbox. A move of user files
///    keeps the sandbox
///  - `can_prompt`: A terminal is available for questions
///  - `yes`: `--yes`: move an old project sandbox without a question
///
/// Returns:
///   `Ok`, or the exit for an error or an aborted question.
pub fn fill_new_sandbox(
    context: &Context,
    lock: &SandboxLock,
    first: &mut FirstStart,
    can_prompt: bool,
    yes: bool,
) -> Result<(), Exit> {
    if !first.is_new() {
        return Ok(());
    }
    let host_cwd = lock.host_cwd();
    if sandbox::lookup::has_legacy_sandbox(host_cwd) {
        ask_import(can_prompt, yes)?;
        let result = migrate::import_legacy(context, lock);
        first.after_import(&result);
        return match result {
            Ok(()) => {
                cli::log!(
                    "  {} {}",
                    cli::check(),
                    messages::start::sandbox_moved(lock.dir())
                );
                Ok(())
            }
            // An older airlock holds the old sandbox. Its text names the
            // other instance, as for a lock that a different instance
            // holds.
            Err(e @ MigrateError::Running) => Err(anyhow::Error::from(e).into()),
            Err(e) => Err(Exit::error(1, e)),
        };
    }
    match migrate::adopt_local_config(host_cwd, lock.dir()) {
        Ok(moved) => {
            if moved {
                first.keep();
            }
            Ok(())
        }
        Err(e) => {
            // The move can fail after some files moved, for example at the
            // remove of `.airlock`. The sandbox then has the only copy.
            first.keep_if_changed(lock);
            Err(Exit::error(1, e))
        }
    }
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
