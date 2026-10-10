//! Move of a project sandbox into the data directory.
//!
//! Moves the sandbox and the local project config out of the `.airlock`
//! directory of the project, and then removes that directory. A directory
//! with files that airlock does not know stays as it is.

use std::path::Path;

use super::{BOXES_DIR, PROJECT_SANDBOX, registry};
use crate::config::files::local_config_names;
use crate::context::Context;
use crate::project::{self, IdleLock};

/// Why a move failed.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// An airlock process uses the sandbox now.
    #[error("the sandbox is in use by another airlock process")]
    Running,
    /// The project sandbox and the data directory are on different file
    /// systems. A rename cannot move the sandbox.
    #[error(
        "cannot move {from} to {to}: they are on different file systems. Set \
         `sandbox_type = \"project-owned\"` in the `[security]` table of \
         ~/.airlock/settings.toml to keep the sandbox in the project, or remove it with \
         `airlock rm`"
    )]
    CrossDevice { from: String, to: String },
    /// `.airlock` holds files that airlock does not know. The move would
    /// remove them.
    #[error(
        ".airlock directory contains extra files ({}), can't migrate - remove extra files or \
         continue using current directory",
        names.join(", ")
    )]
    ExtraFiles { names: Vec<String> },
    /// Another failure.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Files and directories of a stopped sandbox that only a run uses. The
/// move removes them. The next boot makes them again.
const RUNTIME_ENTRIES: [&str; 6] = [
    airlock_common::CLI_SOCK_FILENAME,
    "vsock.sock",
    "ch-api.sock",
    "vfs",
    "overlay/files",
    "pty.dump",
];

/// Entries of `.airlock` that airlock knows, other than the local project
/// config files. The move removes them with the directory.
const KNOWN_ENTRIES: [&str; 3] = [".gitignore", "airlock.log", "sandbox"];

/// Move the project sandbox of `host_cwd` into the data directory and
/// register it. Also moves the local project config into the sandbox
/// directory, and removes the `.airlock` directory of the project.
///
/// Keeps the disk, the image link and the records. The move is one rename,
/// thus the image link stays a hard link. If a different process moved the
/// sandbox first, the result is its id. If `.airlock` holds other files, the
/// move changes nothing and fails with [`MigrateError::ExtraFiles`].
/// Args:
///  - `context`: Process context, for the registry and the data directory
///  - `host_cwd`: Canonical project directory
///
/// Returns:
///   The registry id of the moved sandbox.
pub async fn migrate(context: &Context, host_cwd: &Path) -> Result<String, MigrateError> {
    // Never follow a symlink at `.airlock` or `.airlock/sandbox`. All paths
    // below go through them.
    let airlock = host_cwd.join(".airlock");
    if std::fs::symlink_metadata(&airlock).is_ok_and(|m| !m.is_dir()) {
        return Err(anyhow::anyhow!("{} is not a directory", airlock.display()).into());
    }
    let from = host_cwd.join(PROJECT_SANDBOX);
    if std::fs::symlink_metadata(&from).is_ok_and(|m| !m.is_dir()) {
        return Err(anyhow::anyhow!("{} is not a directory", from.display()).into());
    }
    check_known_entries(&airlock)?;
    // Keep the lock of the old sandbox until the rename is done. Thus no
    // airlock process starts it during the move.
    let held = match project::lock_if_idle(&from) {
        IdleLock::Running => return Err(MigrateError::Running),
        IdleLock::Held(file) => Some(file),
        IdleLock::Missing => None,
    };
    let boxes = context.boxes_dir();
    let existing = registry::list(&context.db).await?;
    if let Some(entry) = existing.iter().find(|e| e.project == host_cwd) {
        return Ok(entry.id.clone());
    }
    if std::fs::symlink_metadata(&from).is_err() {
        return Err(anyhow::anyhow!("{} does not exist", from.display()).into());
    }

    crate::cache::create_private_dir(&boxes)?;
    let id = registry::find_or_register(&context.db, &boxes, host_cwd).await?;
    let to = boxes.join(&id);
    for entry in RUNTIME_ENTRIES {
        remove_entry(&from.join(entry));
    }
    remove_vsock_listeners(&from);
    if let Err(e) = std::fs::rename(&from, &to) {
        registry::unregister(&context.db, &id).await?;
        if e.raw_os_error() == Some(libc::EXDEV) {
            return Err(MigrateError::CrossDevice {
                from: from.display().to_string(),
                to: context.data_dir.join(BOXES_DIR).display().to_string(),
            });
        }
        return Err(anyhow::anyhow!("move {} to {}: {e}", from.display(), to.display()).into());
    }
    drop(held);
    move_local_config(&airlock, &to)?;
    Ok(id)
}

/// Move the local project config of `host_cwd` into the sandbox directory
/// `sandbox_dir` of a new sandbox in the data directory, and remove the
/// `.airlock` directory of the project. Does nothing if there is no local
/// project config.
/// Returns:
///   [`MigrateError::ExtraFiles`] if `.airlock` holds other files. Nothing
///   changes then.
pub fn adopt_local_config(host_cwd: &Path, sandbox_dir: &Path) -> Result<(), MigrateError> {
    if !check_local_config(host_cwd)? {
        return Ok(());
    }
    move_local_config(&host_cwd.join(".airlock"), sandbox_dir)
}

/// Check if [`adopt_local_config`] can move the local project config of
/// `host_cwd`. Changes nothing.
/// Returns:
///   `true` if there is a local project config to move, `false` if there is
///   none, or [`MigrateError::ExtraFiles`] if `.airlock` holds other files.
pub fn check_local_config(host_cwd: &Path) -> Result<bool, MigrateError> {
    let airlock = host_cwd.join(".airlock");
    if !std::fs::symlink_metadata(&airlock).is_ok_and(|m| m.is_dir())
        || local_config_names(&airlock).is_empty()
    {
        return Ok(false);
    }
    check_known_entries(&airlock)?;
    Ok(true)
}

/// Check that the `.airlock` directory `airlock` holds only entries that
/// airlock knows: [`KNOWN_ENTRIES`] and the local project config files.
fn check_known_entries(airlock: &Path) -> Result<(), MigrateError> {
    let Ok(entries) = std::fs::read_dir(airlock) else {
        return Ok(());
    };
    let configs = local_config_names(airlock);
    let mut names: Vec<String> = entries
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !KNOWN_ENTRIES.contains(&n.as_str()) && !configs.contains(n))
        .collect();
    if names.is_empty() {
        return Ok(());
    }
    names.sort();
    Err(MigrateError::ExtraFiles { names })
}

/// Move the local project config files from the `.airlock` directory
/// `airlock` into `sandbox_dir`, and remove `airlock`. Call it only after
/// [`check_known_entries`].
fn move_local_config(airlock: &Path, sandbox_dir: &Path) -> Result<(), MigrateError> {
    for name in local_config_names(airlock) {
        std::fs::rename(airlock.join(&name), sandbox_dir.join(&name)).map_err(|e| {
            anyhow::anyhow!(
                "move {} to {}: {e}",
                airlock.join(&name).display(),
                sandbox_dir.display()
            )
        })?;
    }
    // `.airlock` is a real directory (checked by the callers), so the
    // removal does not follow a link.
    std::fs::remove_dir_all(airlock)
        .map_err(|e| anyhow::anyhow!("remove {}: {e}", airlock.display()))?;
    Ok(())
}

/// Remove the `vsock.sock_<port>` listener sockets of a stopped sandbox.
fn remove_vsock_listeners(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("vsock.sock_")
        {
            remove_entry(&entry.path());
        }
    }
}

/// Remove a file, symlink or directory tree without following symlinks.
/// Ignores errors: the entries are runtime data only.
fn remove_entry(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}
