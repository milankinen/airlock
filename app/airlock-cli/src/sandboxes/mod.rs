//! Sandbox locations.
//!
//! A sandbox keeps its data in the airlock data directory, out of the reach
//! of the sandbox guest. A registry maps each sandbox to its project.
//!
//! Finds the sandbox of a directory (and of its parents, if necessary). The
//! search can also go from a git worktree to its main repository. Also
//! imports a sandbox that an older airlock version kept in the project
//! directory, and removes sandboxes.

mod migrate;
pub mod registry;
mod remove;

pub use self::migrate::{MigrateError, adopt_local_config, check_local_config, import_legacy};
pub use self::remove::remove_box;
use crate::context::Context;
use crate::packs::install::state::STATE_FILE;
use crate::project::SandboxLock;
use crate::util::PinnedDir;
use crate::util::verified_path::VerifiedPath;
use crate::vm::disk;

/// Directory of the sandboxes in the data directory.
pub const BOXES_DIR: &str = "boxes";

/// Path of the sandbox of an older airlock version, relative to the
/// project directory. Only the import uses it.
pub const PROJECT_SANDBOX: &str = ".airlock/sandbox";

/// A sandbox that [`resolve_sandbox`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Registry id of the sandbox.
    pub id: String,
    /// Canonical project directory.
    pub project: VerifiedPath,
    /// Sandbox directory. It can be missing for a registered sandbox whose
    /// directory was deleted.
    pub dir: VerifiedPath,
}

/// Find all sandboxes of `start`, or of `start` and its parents.
/// Args:
///  - `context`: Process context, for the registry and the data directory
///  - `start`: Canonical directory to start from
///  - `parents`: Also look in the parent directories of `start`
///  - `follow_git_worktree`: When the search gets to the root of a git
///    worktree, continue at the root of the main repository of the worktree
///
/// Returns:
///   The registered sandboxes, nearest directory first.
pub async fn candidates(
    context: &Context,
    start: &VerifiedPath,
    parents: bool,
    follow_git_worktree: bool,
) -> anyhow::Result<Vec<Found>> {
    let entries = registry::list(&context.db).await?;
    let boxes = &context.data.sandboxes;
    let mut found = Vec::new();
    for dir in search_dirs(start, parents, follow_git_worktree) {
        // The registry has the canonical paths. A dir that changed since
        // the check has no sandbox.
        let Ok(path) = dir.to_entry_path() else {
            continue;
        };
        let path = path.as_os_str();
        for entry in entries.iter().filter(|e| e.project == path) {
            // A bad entry must not stop the lookup of the other sandboxes.
            let sandbox_dir = match boxes.join(&entry.id) {
                Ok(sandbox_dir) => sandbox_dir,
                Err(e) => {
                    tracing::debug!("skip sandbox {}: {e}", entry.id);
                    continue;
                }
            };
            found.push(Found {
                id: entry.id.clone(),
                project: dir.clone(),
                dir: sandbox_dir,
            });
        }
    }
    Ok(found)
}

/// Get the directories that [`candidates`] looks in, in search order.
fn search_dirs(
    start: &VerifiedPath,
    parents: bool,
    follow_git_worktree: bool,
) -> Vec<VerifiedPath> {
    let mut dirs: Vec<VerifiedPath> = Vec::new();
    let mut next = Some(start.clone());
    while let Some(dir) = next {
        // A worktree link can point back into the visited directories.
        if dirs.contains(&dir) {
            break;
        }
        next = follow_git_worktree
            .then(|| git_worktree_main(&dir))
            .flatten();
        if next.is_none() && parents {
            next = dir.parent();
        }
        dirs.push(dir);
    }
    dirs
}

/// Get the canonical root of the main repository, if `dir` is the root of
/// a linked git worktree of it. A bare main repository has no root.
// A linked worktree has a `.git` file with `gitdir: <common>/worktrees/<n>`.
// That directory has a `commondir` file with the path of `<common>`, and a
// `gitdir` file with the path of `<worktree>/.git`. A submodule also has a
// `.git` file, but its git directory is not in `worktrees/`.
//
// Code in a sandbox can write the `.git` file of a worktree in its project
// mount, and it can point the file to any directory. The `gitdir` file that
// links back is in the main repository, out of reach of that code. Thus the
// link back must name `dir/.git`, else `dir` is not a worktree of that
// repository. The `.git` file must not be a symlink: a symlink to the
// `.git` file of a real worktree would pass the link back check.
//
// The git paths can go through symlinks, for example a linked home dir.
// Thus the compare resolves them with symlinks followed. The reads of the
// git files refuse a symlink at the file.
fn git_worktree_main(dir: &VerifiedPath) -> Option<VerifiedPath> {
    let dot_git = read_git_file(dir, ".git")?;
    let gitdir = dir
        .join_follow(dot_git.strip_prefix("gitdir:")?.trim(), None)
        .ok()?;
    let worktrees = gitdir.parent()?;
    let common = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || common.file_name()? != ".git" {
        return None;
    }
    let commondir = read_git_file(&gitdir, "commondir")?;
    if gitdir.join_follow(commondir, None).ok()? != common {
        return None;
    }
    // Git 2.48+ can write a path relative to the git directory. `dir` is
    // canonical, thus `dir/.git` is canonical too.
    let back = read_git_file(&gitdir, "gitdir")?;
    if gitdir.join_follow(back, None).ok()? != dir.join(".git").ok()? {
        return None;
    }
    common.parent()
}

/// Read the git file `name` in `dir` (see [`PinnedDir::read`]).
/// Returns:
///   The trimmed text, or `None` if the file does not exist, is a symlink,
///   is not a regular file, or is not valid.
fn read_git_file(dir: &VerifiedPath, name: &str) -> Option<String> {
    // The git link files hold one path.
    const CAP: u64 = 4096;
    let bytes = PinnedDir::pin(dir.to_path().ok()?)
        .ok()?
        .read(name, CAP)
        .ok()??;
    Some(String::from_utf8(bytes).ok()?.trim().to_string())
}

/// Find the sandbox of `start`, or of the nearest of `start` and its
/// parents (see [`candidates`] for the arguments).
/// Returns:
///   The sandbox, or `None` if there is none.
pub async fn resolve_sandbox(
    context: &Context,
    start: &VerifiedPath,
    parents: bool,
    follow_git_worktree: bool,
) -> anyhow::Result<Option<Found>> {
    Ok(candidates(context, start, parents, follow_git_worktree)
        .await?
        .into_iter()
        .next())
}

/// Take the lock of the registered sandbox `id` of the project `host_cwd`
/// in the data directory.
/// Returns:
///   The held lock, or `None` if the sandbox left the registry before the
///   lock. Error if the lock fails.
pub async fn lock_box(
    context: &Context,
    host_cwd: &VerifiedPath,
    id: &str,
) -> anyhow::Result<Option<SandboxLock>> {
    // `airlock sandboxes rm` can remove the sandbox between the registry
    // lookup and the lock. The lock then makes a new empty directory. Check
    // the registry again under the lock, and remove that directory.
    let lock = SandboxLock::acquire_box(host_cwd, &context.data.sandboxes, id)?;
    let cwd = host_cwd.to_entry_path()?;
    if registry::get(&context.db, id).await?.as_deref() != Some(cwd.as_os_str()) {
        let dir = lock.dir().clone();
        drop(lock);
        let _ = dir.remove_dir_all();
        return Ok(None);
    }
    Ok(Some(lock))
}

/// Get the project directory of the current directory `work`, for the
/// sandbox search. The registry has canonical project paths.
/// Returns:
///   The current directory with its symlinks resolved, or error if a part
///   of it changed since the start of the process.
pub(crate) fn project_dir(work: &VerifiedPath) -> std::io::Result<VerifiedPath> {
    work.reverify()
}

/// Get the directory of the local project config (`airlock.<ext>`) of a
/// sandbox. The sandbox keeps it in the sandbox directory, out of the
/// repository.
/// Args:
///  - `sandbox_dir`: Sandbox directory of the project
///
/// Returns:
///   The directory, or error if a part of `sandbox_dir` changed.
pub fn local_config_dir(sandbox_dir: &VerifiedPath) -> std::io::Result<VerifiedPath> {
    sandbox_dir.to_entry_path()?;
    Ok(sandbox_dir.clone())
}

/// Find the directory of the local project config of the project
/// `host_cwd` (see [`local_config_dir`]). A project without a sandbox uses
/// `<host_cwd>/.airlock`.
pub async fn find_local_config_dir(
    context: &Context,
    host_cwd: &VerifiedPath,
) -> anyhow::Result<VerifiedPath> {
    Ok(
        match resolve_sandbox(context, host_cwd, false, false).await? {
            Some(found) => local_config_dir(&found.dir)?,
            None => host_cwd.join(".airlock")?,
        },
    )
}

/// Check if the project `host_cwd` has a sandbox with content: a disk or
/// install records. An empty sandbox directory does not count. A sandbox
/// of an older version in the project counts: the start imports it.
pub async fn has_content(context: &Context, host_cwd: &VerifiedPath) -> anyhow::Result<bool> {
    if has_legacy_sandbox(host_cwd) {
        return Ok(true);
    }
    let Some(found) = resolve_sandbox(context, host_cwd, false, false).await? else {
        return Ok(false);
    };
    let exists = |name: &str| found.dir.join(name).and_then(|p| p.metadata()).is_ok();
    Ok(exists(disk::DISK_FILE) || exists(STATE_FILE))
}

/// Check if the project `host_cwd` has a sandbox of an older version at
/// `.airlock/sandbox`. Any entry counts, also a symlink: the import
/// refuses a bad entry with an error.
pub fn has_legacy_sandbox(host_cwd: &VerifiedPath) -> bool {
    host_cwd
        .join(PROJECT_SANDBOX)
        .and_then(|p| p.symlink_metadata())
        .is_ok()
}

#[cfg(test)]
mod tests;
