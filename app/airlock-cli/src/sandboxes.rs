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
//! both places. Also moves a project sandbox into the data directory, and
//! removes sandboxes.

mod migrate;
pub mod registry;
mod remove;

use std::path::{Path, PathBuf};

pub use self::migrate::{MigrateError, adopt_local_config, check_local_config, migrate};
pub use self::remove::remove_box;
use crate::context::Context;
use crate::packs::install::state::STATE_FILE;
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
///
/// Returns:
///   The sandboxes, nearest directory first. For one directory, a
///   registered sandbox comes before a project sandbox.
pub async fn candidates(
    context: &Context,
    start: &Path,
    parents: bool,
) -> anyhow::Result<Vec<Found>> {
    let entries = registry::list(&context.db).await?;
    let boxes = context.boxes_dir();
    let dirs: Vec<&Path> = if parents {
        start.ancestors().collect()
    } else {
        vec![start]
    };
    let mut found = Vec::new();
    for dir in dirs {
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

/// Find the sandbox of `start`, or of the nearest of `start` and its
/// parents (see [`candidates`]).
/// Returns:
///   The sandbox, or `None` if there is none.
pub async fn resolve_sandbox(
    context: &Context,
    start: &Path,
    parents: bool,
) -> anyhow::Result<Option<Found>> {
    Ok(candidates(context, start, parents)
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
    Ok(match resolve_sandbox(context, host_cwd, false).await? {
        Some(found) => local_config_dir(host_cwd, &found.dir),
        None => host_cwd.join(".airlock"),
    })
}

/// Check if the project `host_cwd` has a sandbox with content: a disk or
/// install records. An empty sandbox directory does not count.
pub async fn has_content(context: &Context, host_cwd: &Path) -> anyhow::Result<bool> {
    let Some(found) = resolve_sandbox(context, host_cwd, false).await? else {
        return Ok(false);
    };
    Ok(found.dir.join(disk::DISK_FILE).exists() || found.dir.join(STATE_FILE).exists())
}

#[cfg(test)]
mod tests;
