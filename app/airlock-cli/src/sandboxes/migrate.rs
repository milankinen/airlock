//! Move of a project sandbox into the data directory.
//!
//! Moves the sandbox and the local project config out of the `.airlock`
//! directory of the project, and then removes that directory. A directory
//! with files that airlock does not know stays as it is.

use super::registry;
use crate::config::files::local_config_names;
use crate::context::Context;
use crate::project::{self, IdleLock};
use crate::util::verified_path::VerifiedPath;

/// Why a move failed.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// An airlock process uses the sandbox now.
    #[error("{}", crate::messages::sandboxes::SANDBOX_IN_USE)]
    Running,
    /// The project sandbox and the data directory are on different file
    /// systems. A rename cannot move the sandbox.
    #[error("{}", crate::messages::sandboxes::cross_device(from, to))]
    CrossDevice { from: String, to: String },
    /// `.airlock` holds files that airlock does not know. The move would
    /// remove them.
    #[error("{}", crate::messages::sandboxes::extra_files(names))]
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
pub async fn migrate(context: &Context, host_cwd: &VerifiedPath) -> Result<String, MigrateError> {
    // Never follow a symlink at `.airlock` or `.airlock/sandbox`. All paths
    // below go through them.
    let airlock = airlock_dir(host_cwd)?;
    if airlock.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
        return Err(anyhow::anyhow!(crate::messages::sandboxes::not_a_directory(&airlock)).into());
    }
    let from = airlock.join("sandbox").map_err(anyhow::Error::from)?;
    if from.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
        return Err(anyhow::anyhow!(crate::messages::sandboxes::not_a_directory(&from)).into());
    }
    check_known_entries(&airlock)?;
    // Keep the lock of the old sandbox until the rename is done. Thus no
    // airlock process starts it during the move.
    let held = match project::lock_if_idle(&from) {
        IdleLock::Running => return Err(MigrateError::Running),
        IdleLock::Held(file) => Some(file),
        IdleLock::Missing => None,
    };
    let boxes = &context.data.sandboxes;
    let existing = registry::list(&context.db).await?;
    let project = host_cwd.to_entry_path().map_err(anyhow::Error::from)?;
    if let Some(entry) = existing.iter().find(|e| e.project == project.as_os_str()) {
        return Ok(entry.id.clone());
    }
    if from.symlink_metadata().is_err() {
        return Err(anyhow::anyhow!(crate::messages::sandboxes::does_not_exist(&from)).into());
    }

    let boxes = &crate::data_dir::create_private_dir(boxes)?;
    let id = registry::find_or_register(&context.db, boxes, host_cwd).await?;
    let to = boxes.join(&id).map_err(anyhow::Error::from)?;
    for entry in RUNTIME_ENTRIES {
        if let Ok(path) = from.join(entry) {
            remove_entry(&path);
        }
    }
    remove_vsock_listeners(&from);
    if let Err(e) = from.rename_to(&to) {
        registry::unregister(&context.db, &id).await?;
        if e.raw_os_error() == Some(libc::EXDEV) {
            return Err(MigrateError::CrossDevice {
                from: from.to_string(),
                to: context.data.sandboxes.to_string(),
            });
        }
        return Err(anyhow::anyhow!(crate::messages::sandboxes::move_failed(&from, &to, e)).into());
    }
    drop(held);
    move_local_config(&airlock, &to)?;
    Ok(id)
}

/// Get the `.airlock` directory of the project `host_cwd`.
fn airlock_dir(host_cwd: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    Ok(host_cwd.join(".airlock")?)
}

/// Move the local project config of `host_cwd` into the sandbox directory
/// `sandbox_dir` of a new sandbox in the data directory, and remove the
/// `.airlock` directory of the project. Does nothing if there is no local
/// project config.
/// Args:
///  - `host_cwd`: Canonical project directory
///  - `sandbox_dir`: Sandbox directory of the new sandbox
///
/// Returns:
///   [`MigrateError::ExtraFiles`] if `.airlock` holds other files. Nothing
///   changes then.
pub fn adopt_local_config(
    host_cwd: &VerifiedPath,
    sandbox_dir: &VerifiedPath,
) -> Result<(), MigrateError> {
    if !check_local_config(host_cwd)? {
        return Ok(());
    }
    move_local_config(&airlock_dir(host_cwd)?, sandbox_dir)?;
    Ok(())
}

/// Check if [`adopt_local_config`] can move the local project config of
/// `host_cwd`. Changes nothing.
/// Returns:
///   `true` if there is a local project config to move, `false` if there is
///   none, or [`MigrateError::ExtraFiles`] if `.airlock` holds other files.
pub fn check_local_config(host_cwd: &VerifiedPath) -> Result<bool, MigrateError> {
    let airlock = airlock_dir(host_cwd)?;
    if !airlock.symlink_metadata().is_ok_and(|m| m.is_dir())
        || local_config_names(&airlock).is_empty()
    {
        return Ok(false);
    }
    check_known_entries(&airlock)?;
    Ok(true)
}

/// Check that the `.airlock` directory `airlock` holds only entries that
/// airlock knows: [`KNOWN_ENTRIES`] and the local project config files.
fn check_known_entries(airlock: &VerifiedPath) -> Result<(), MigrateError> {
    let Ok(entries) = airlock.read_dir() else {
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
fn move_local_config(airlock: &VerifiedPath, sandbox_dir: &VerifiedPath) -> anyhow::Result<()> {
    for name in local_config_names(airlock) {
        let (from, to) = (airlock.join(&name)?, sandbox_dir.join(&name)?);
        from.rename_to(&to).map_err(|e| {
            anyhow::anyhow!(crate::messages::sandboxes::move_failed(
                &from,
                sandbox_dir,
                e
            ))
        })?;
    }
    // The check before use refuses a symlink in place of `.airlock`. A
    // symlink at the last name is removed, not followed.
    airlock
        .remove_dir_all()
        .map_err(|e| anyhow::anyhow!(crate::messages::sandboxes::remove_failed(airlock, e)))?;
    Ok(())
}

/// Remove the `vsock.sock_<port>` listener sockets of a stopped sandbox.
fn remove_vsock_listeners(dir: &VerifiedPath) {
    let Ok(entries) = dir.read_dir() else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with("vsock.sock_")
            && let Ok(path) = dir.join(entry.file_name())
        {
            remove_entry(&path);
        }
    }
}

/// Remove a file, symlink or directory tree without following symlinks.
/// Ignores errors: the entries are runtime data only.
fn remove_entry(path: &VerifiedPath) {
    match path.symlink_metadata() {
        Ok(meta) if meta.is_dir() => {
            let _ = path.remove_dir_all();
        }
        Ok(_) => {
            let _ = path.remove_file();
        }
        Err(_) => {}
    }
}
