//! Sandbox lookup.
//!
//! Finds the sandbox of a directory, and of its parents if necessary. The
//! search can also go from a git worktree to its main repository. Never
//! follows symlinks in the sandbox paths, because code in a sandbox can
//! write its project mount. Also gives the guest directory of a host
//! directory in the project.

use std::ffi::{OsStr, OsString};

use super::meta::read_run_meta;
use super::registry;
use crate::context::Context;
use crate::packs::install::state::STATE_FILE;
use crate::util::PinnedDir;
use crate::util::verified_path::VerifiedPath;
use crate::vm::disk;
use crate::{cli, messages};

/// Directory of the sandboxes in the data directory.
pub const BOXES_DIR: &str = "boxes";

/// Path of the sandbox of an older airlock version, relative to the
/// project directory. Only the import uses it.
pub const PROJECT_SANDBOX: &str = ".airlock/sandbox";

/// A registered sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sandbox {
    /// Registry id of the sandbox.
    pub(super) id: String,
    /// Project directory as the registry has it.
    pub(super) path: OsString,
    /// Canonical project directory, or the reason why the registered path
    /// fails the path check (for example a symlink in it).
    pub(super) project: Result<VerifiedPath, String>,
    /// Sandbox directory. It can be missing for a registered sandbox whose
    /// directory was deleted.
    pub(super) dir: VerifiedPath,
}

/// State of a sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    /// A process holds a lock of the sandbox: it starts, runs or stops.
    Running,
    /// No process uses the sandbox.
    Stopped,
    /// The sandbox has no directory.
    Missing,
}

impl Sandbox {
    /// Make the value for the sandbox `id` of the canonical project
    /// directory `project`, with its sandbox directory `dir`.
    pub(crate) fn new(id: String, project: VerifiedPath, dir: VerifiedPath) -> Self {
        Self {
            id,
            path: project.given().as_os_str().to_owned(),
            project: Ok(project),
            dir,
        }
    }

    /// Get the registry id.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the project directory as the registry has it.
    pub fn path(&self) -> &OsStr {
        &self.path
    }

    /// Get the canonical project directory.
    /// Returns:
    ///   The directory, or error if the registered path fails the path
    ///   check.
    pub fn project(&self) -> anyhow::Result<&VerifiedPath> {
        self.project
            .as_ref()
            .map_err(|e| anyhow::anyhow!("{}: {e}", self.path.display()))
    }

    /// Get the sandbox directory.
    pub fn dir(&self) -> &VerifiedPath {
        &self.dir
    }

    /// Get the state of the sandbox from its directory and its locks.
    pub fn status(&self) -> Status {
        if self.dir.symlink_metadata().is_err() {
            Status::Missing
        } else if super::lock::is_running(&self.dir) {
            Status::Running
        } else {
            Status::Stopped
        }
    }

    /// Check if a process uses the sandbox now (see [`Status::Running`]).
    pub fn is_running(&self) -> bool {
        super::lock::is_running(&self.dir)
    }

    /// Check if a client can start the sandbox with the config of the last
    /// `airlock start`: it has content and that config.
    pub(super) fn is_startable(&self) -> bool {
        dir_has_content(&self.dir) && read_run_meta(&self.dir).last_run_config.is_some()
    }

    /// Get the directory of the local project config (`airlock.<ext>`).
    /// The sandbox keeps it in the sandbox directory, out of the
    /// repository.
    /// Returns:
    ///   The directory, or error if a part of the sandbox directory
    ///   changed.
    pub fn local_config_dir(&self) -> std::io::Result<VerifiedPath> {
        local_config_dir(&self.dir)
    }

    /// Get the project dir in the guest: the guest cwd of the last
    /// `airlock start`.
    /// Returns:
    ///   The guest dir, or error if the registered path fails the path
    ///   check.
    // A sandbox of an older airlock has no guest cwd record and uses the
    // host path.
    pub fn guest_root(&self) -> anyhow::Result<String> {
        let project = self.project()?;
        Ok(read_run_meta(&self.dir)
            .guest_cwd
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| project.to_string()))
    }

    /// Get the guest dir of the host dir `host`.
    /// Returns:
    ///   For a dir in the project, the same dir below the project dir of
    ///   the guest. For other dirs, the host path with its symlinks
    ///   resolved. Error if a part of `host` changed since the check.
    // The project mount of the guest is at the guest cwd of `airlock
    // start`, which can differ from the host path (`--sandbox-cwd`). A dir
    // out of the project, for example a git worktree of the project, has
    // no project mount. A user mount at the same path can have it.
    pub fn guest_dir(&self, host: &VerifiedPath) -> anyhow::Result<String> {
        let project = self.project()?;
        let host = host.reverify()?;
        let root = self.guest_root()?;
        Ok(match host.relative_to(project) {
            Some(rel) if rel.is_empty() => root,
            Some(rel) => format!("{}/{rel}", root.trim_end_matches('/')),
            None => host.to_path()?.to_string_lossy().into_owned(),
        })
    }
}

/// Options of [`find`].
#[derive(Debug, Clone, Copy, Default)]
pub struct FindOpts {
    /// Also look in the parent directories.
    pub parents: bool,
    /// When the search gets to the root of a git worktree, continue at the
    /// root of the main repository of the worktree.
    pub follow_git_worktree: bool,
}

/// Find the nearest registered sandbox of the directory `work`. Checks no
/// other condition: the sandbox can run or not.
/// Args:
///  - `context`: Process context, for the registry and the data directory
///  - `work`: Directory to start from, for example the current directory
///  - `opts`: Where the search looks
///
/// Returns:
///   The sandbox, or `None` if there is none. Error if a part of `work`
///   changed since its check, or the registry does not read.
pub async fn find(
    context: &Context,
    work: &VerifiedPath,
    opts: FindOpts,
) -> anyhow::Result<Option<Sandbox>> {
    // The registry has canonical project paths.
    let start = work.reverify()?;
    let found = candidates(context, &start, opts.parents, opts.follow_git_worktree)
        .await?
        .into_iter()
        .next();
    // A sandbox out of the start directory and its parents comes from a
    // worktree link. Tell the user which sandbox it is.
    if let Some(Ok(project)) = found.as_ref().map(|s| &s.project)
        && !start.starts_with(project)
    {
        cli::log!(
            "{} {}",
            cli::bullet(),
            messages::cli::attached_to_sandbox(project)
        );
    }
    Ok(found)
}

/// Get the registered sandbox `id`.
/// Returns:
///   The sandbox, or error if `id` is not registered.
pub async fn get(context: &Context, id: &str) -> anyhow::Result<Sandbox> {
    let path = if registry::is_valid_id(id) {
        registry::get(&context.db, id).await?
    } else {
        None
    };
    let path = path.ok_or_else(|| anyhow::anyhow!(messages::cli::no_sandbox_with_id(id)))?;
    from_entry(context, id.to_string(), path)
}

/// Get all registered sandboxes.
pub async fn list(context: &Context) -> anyhow::Result<Vec<Sandbox>> {
    registry::list(&context.db)
        .await?
        .into_iter()
        .map(|entry| from_entry(context, entry.id, entry.project))
        .collect()
}

/// Make the value for the registry entry `id` of the project `path`.
fn from_entry(context: &Context, id: String, path: OsString) -> anyhow::Result<Sandbox> {
    let project = VerifiedPath::new(&path, &context.roots()).map_err(|e| e.to_string());
    let dir = context.data.sandboxes.join(&id)?;
    Ok(Sandbox {
        id,
        path,
        project,
        dir,
    })
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
pub(super) async fn candidates(
    context: &Context,
    start: &VerifiedPath,
    parents: bool,
    follow_git_worktree: bool,
) -> anyhow::Result<Vec<Sandbox>> {
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
            found.push(Sandbox {
                id: entry.id.clone(),
                path: entry.project.clone(),
                project: Ok(dir.clone()),
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

/// Get the directory of the local project config of the sandbox directory
/// `sandbox_dir` (see [`Sandbox::local_config_dir`]).
pub fn local_config_dir(sandbox_dir: &VerifiedPath) -> std::io::Result<VerifiedPath> {
    sandbox_dir.to_entry_path()?;
    Ok(sandbox_dir.clone())
}

/// Check if the sandbox directory `dir` has content: a disk or install
/// records.
pub fn dir_has_content(dir: &VerifiedPath) -> bool {
    let exists = |name: &str| dir.join(name).and_then(|p| p.metadata()).is_ok();
    exists(disk::DISK_FILE) || exists(STATE_FILE)
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
