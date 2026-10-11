//! Sandbox locations.
//!
//! A sandbox keeps its data in one of two places:
//!  * the airlock data directory, out of the reach of the sandbox guest. A
//!    registry maps each of these sandboxes to its project.
//!  * the `.airlock/sandbox` directory of the project. Older airlock
//!    versions used only this place, and a user setting can still select
//!    it. These sandboxes are not in the registry.
//!
//! Finds the sandbox of a directory (and of its parents, if necessary) in
//! both places. The search can also go from a git worktree to its main
//! repository. Also moves a project sandbox into the data directory, and
//! removes sandboxes.

mod migrate;
pub mod registry;
mod remove;

use std::path::{Path, PathBuf};

pub use self::migrate::{MigrateError, adopt_local_config, check_local_config, migrate};
pub use self::remove::remove_box;
use crate::context::Context;
use crate::packs::install::state::STATE_FILE;
use crate::util::PinnedDir;
use crate::vm::disk;

/// Directory of the sandboxes in the data directory.
pub const BOXES_DIR: &str = "boxes";

/// Path of a project sandbox, relative to the project directory.
pub const PROJECT_SANDBOX: &str = ".airlock/sandbox";

/// Marker file in a project sandbox: the user chose to keep the sandbox in
/// the project, so `airlock start` does not offer to move it again.
pub const KEEP_IN_PROJECT: &str = "keep-in-project";

/// Where a sandbox keeps its data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Location {
    /// In the data directory, with the registry id.
    DataDir { id: String },
    /// In the `.airlock/sandbox` directory of the project.
    ProjectDir,
}

/// A sandbox that [`resolve_sandbox`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    /// Where the sandbox keeps its data.
    pub location: Location,
    /// Canonical project directory.
    pub project: PathBuf,
    /// Sandbox directory. It can be missing for a registered sandbox whose
    /// directory was deleted.
    pub dir: PathBuf,
}

impl Found {
    /// Get the registry id, or `None` for a project sandbox.
    pub fn id(&self) -> Option<&str> {
        match &self.location {
            Location::DataDir { id } => Some(id),
            Location::ProjectDir => None,
        }
    }
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
///   The sandboxes, nearest directory first. For one directory, a
///   registered sandbox comes before a project sandbox.
pub async fn candidates(
    context: &Context,
    start: &Path,
    parents: bool,
    follow_git_worktree: bool,
) -> anyhow::Result<Vec<Found>> {
    let entries = registry::list(&context.db).await?;
    let boxes = context.boxes_dir();
    let mut found = Vec::new();
    for dir in search_dirs(start, parents, follow_git_worktree) {
        let dir = dir.as_path();
        for entry in entries.iter().filter(|e| e.project == dir) {
            found.push(Found {
                location: Location::DataDir {
                    id: entry.id.clone(),
                },
                project: dir.to_path_buf(),
                dir: boxes.join(&entry.id),
            });
        }
        // Any entry counts, also a symlink. The users of the sandbox
        // directory refuse a symlink with a clear message.
        let project_dir = dir.join(PROJECT_SANDBOX);
        if std::fs::symlink_metadata(&project_dir).is_ok() {
            found.push(Found {
                location: Location::ProjectDir,
                project: dir.to_path_buf(),
                dir: project_dir,
            });
        }
    }
    Ok(found)
}

/// Get the directories that [`candidates`] looks in, in search order.
fn search_dirs(start: &Path, parents: bool, follow_git_worktree: bool) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = Vec::new();
    let mut next = Some(start.to_path_buf());
    while let Some(dir) = next {
        // A worktree link can point back into the visited directories.
        if dirs.contains(&dir) {
            break;
        }
        next = follow_git_worktree
            .then(|| git_worktree_main(&dir))
            .flatten();
        if next.is_none() && parents {
            next = dir.parent().map(Path::to_path_buf);
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
fn git_worktree_main(dir: &Path) -> Option<PathBuf> {
    let dot_git = read_git_file(dir, ".git")?;
    let gitdir = std::fs::canonicalize(dir.join(dot_git.strip_prefix("gitdir:")?.trim())).ok()?;
    let worktrees = gitdir.parent()?;
    let common = worktrees.parent()?;
    if worktrees.file_name()? != "worktrees" || common.file_name()? != ".git" {
        return None;
    }
    let commondir = read_git_file(&gitdir, "commondir")?;
    if std::fs::canonicalize(gitdir.join(commondir)).ok()? != common {
        return None;
    }
    // Git 2.48+ can write a path relative to the git directory. `dir` is
    // canonical, thus `dir/.git` is canonical too.
    let back = read_git_file(&gitdir, "gitdir")?;
    if std::fs::canonicalize(gitdir.join(back)).ok()? != dir.join(".git") {
        return None;
    }
    common.parent().map(Path::to_path_buf)
}

/// Read the git file `name` in `dir` (see [`PinnedDir::read`]).
/// Returns:
///   The trimmed text, or `None` if the file does not exist, is a symlink,
///   is not a regular file, or is not valid.
fn read_git_file(dir: &Path, name: &str) -> Option<String> {
    // The git link files hold one path.
    const CAP: u64 = 4096;
    let bytes = PinnedDir::pin(dir).ok()?.read(name, CAP).ok()??;
    Some(String::from_utf8(bytes).ok()?.trim().to_string())
}

/// Find the sandbox of `start`, or of the nearest of `start` and its
/// parents (see [`candidates`] for the arguments).
/// Returns:
///   The sandbox, or `None` if there is none.
pub async fn resolve_sandbox(
    context: &Context,
    start: &Path,
    parents: bool,
    follow_git_worktree: bool,
) -> anyhow::Result<Option<Found>> {
    Ok(candidates(context, start, parents, follow_git_worktree)
        .await?
        .into_iter()
        .next())
}

/// Get the directory of the local project config (`airlock.<ext>`) of a
/// sandbox. A sandbox in the data directory keeps it in the sandbox
/// directory, out of the repository. A project sandbox keeps it in
/// `<host_cwd>/.airlock`.
/// Args:
///  - `host_cwd`: Canonical project directory
///  - `sandbox_dir`: Sandbox directory of the project
pub fn local_config_dir(host_cwd: &Path, sandbox_dir: &Path) -> PathBuf {
    if sandbox_dir == host_cwd.join(PROJECT_SANDBOX) {
        host_cwd.join(".airlock")
    } else {
        sandbox_dir.to_path_buf()
    }
}

/// Find the directory of the local project config of the project
/// `host_cwd` (see [`local_config_dir`]). A project without a sandbox uses
/// `<host_cwd>/.airlock`.
pub async fn find_local_config_dir(context: &Context, host_cwd: &Path) -> anyhow::Result<PathBuf> {
    Ok(
        match resolve_sandbox(context, host_cwd, false, false).await? {
            Some(found) => local_config_dir(host_cwd, &found.dir),
            None => host_cwd.join(".airlock"),
        },
    )
}

/// Check if the project `host_cwd` has a sandbox with content: a disk or
/// install records. An empty sandbox directory does not count.
pub async fn has_content(context: &Context, host_cwd: &Path) -> anyhow::Result<bool> {
    let Some(found) = resolve_sandbox(context, host_cwd, false, false).await? else {
        return Ok(false);
    };
    Ok(found.dir.join(disk::DISK_FILE).exists() || found.dir.join(STATE_FILE).exists())
}

#[cfg(test)]
mod tests;
