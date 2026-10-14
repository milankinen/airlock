//! Import of a project sandbox into the data directory.
//!
//! Older airlock versions kept the sandbox in the `.airlock` directory of
//! the project, where the sandbox guest can write. Thus the import trusts no
//! content of the old sandbox. It takes only the known files, and only as
//! regular files with one link that the user owns. It checks the content of
//! each file. It takes the image only as a reference to the image cache.
//! All other entries stay and go with the old directory.
//!
//! Also moves the local project config out of the `.airlock` directory of
//! the project, and then removes that directory. A directory with files that
//! airlock does not know stays as it is.

use std::fs::File;
use std::os::unix::fs::{MetadataExt, PermissionsExt};

use super::{PROJECT_SANDBOX, registry};
use crate::config::files::local_config_names;
use crate::context::Context;
use crate::packs::install::state::{self, PackStatus, ReadState, STATE_CAP, STATE_FILE};
use crate::project::{self, SandboxLock};
use crate::util::PinnedDir;
use crate::util::clone::clone_from_file;
use crate::util::verified_path::VerifiedPath;
use crate::vm::disk::{self, DISK_FILE, DISK_ID_CAP, DISK_ID_FILE};
use crate::{cli, messages, oci};

/// Why an import failed.
#[derive(Debug, thiserror::Error)]
pub enum MigrateError {
    /// An airlock process uses the sandbox now.
    #[error("{}", crate::messages::sandboxes::SANDBOX_IN_USE)]
    Running,
    /// `.airlock` holds files that airlock does not know. The import would
    /// remove them.
    #[error("{}", crate::messages::sandboxes::extra_files(names))]
    ExtraFiles { names: Vec<String> },
    /// Another failure.
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

/// Entries of `.airlock` that airlock knows, other than the local project
/// config files. The import removes them with the directory.
const KNOWN_ENTRIES: [&str; 3] = [".gitignore", "airlock.log", "sandbox"];

/// A test hook of the import.
#[cfg(test)]
pub(crate) type Hook = Box<dyn FnOnce() -> anyhow::Result<()>>;

#[cfg(test)]
thread_local! {
    /// Runs in the next import of this thread after the disk opens and
    /// before it moves. Only for tests of a swap of the disk.
    pub(crate) static BEFORE_DISK_MOVE: std::cell::RefCell<Option<Hook>> =
        const { std::cell::RefCell::new(None) };
    /// Runs in the next import of this thread after it took the files of
    /// the old sandbox. An error makes the import fail. Only for tests of
    /// the rollback.
    pub(crate) static AFTER_TAKE: std::cell::RefCell<Option<Hook>> =
        const { std::cell::RefCell::new(None) };
}

/// Run the test hook `hook`, if it is set.
#[cfg(test)]
fn run_hook(
    hook: &'static std::thread::LocalKey<std::cell::RefCell<Option<Hook>>>,
) -> anyhow::Result<()> {
    hook.take().map_or(Ok(()), |f| f())
}

/// Import the project sandbox of `host_cwd` into the data directory and
/// register it. Also moves the local project config into the new sandbox
/// directory, and removes the `.airlock` directory of the project.
///
/// Takes only the disk with its identity and records, the CA, and the image
/// reference (see the module doc). If the registry has the project already
/// (for example a different process imported it first), the import takes
/// nothing and gives the lock of that sandbox. If `.airlock` holds other
/// files, the import changes nothing and fails with
/// [`MigrateError::ExtraFiles`]. If the import fails, the old sandbox stays.
/// Args:
///  - `context`: Process context, for the registry and the data directory
///  - `host_cwd`: Canonical project directory
///
/// Returns:
///   The held lock of the sandbox in the data directory.
pub async fn import_legacy(
    context: &Context,
    host_cwd: &VerifiedPath,
) -> Result<SandboxLock, MigrateError> {
    // Never follow a symlink at `.airlock` or `.airlock/sandbox`.
    let airlock = airlock_dir(host_cwd)?;
    if airlock.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
        return Err(anyhow::anyhow!(messages::sandboxes::not_a_directory(&airlock)).into());
    }
    let from = airlock.join("sandbox").map_err(anyhow::Error::from)?;
    if from.symlink_metadata().is_ok_and(|m| !m.is_dir()) {
        return Err(anyhow::anyhow!(messages::sandboxes::not_a_directory(&from)).into());
    }
    check_known_entries(&airlock)?;
    // Each part of the path opens with `O_NOFOLLOW` and an owner check. All
    // later reads go through this descriptor, thus a swap of a directory
    // cannot redirect them.
    let root = host_cwd.to_path().map_err(anyhow::Error::from)?;
    let old = match PinnedDir::open(root, PROJECT_SANDBOX.as_ref(), false) {
        Ok(old) => Some(old),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            return Err(anyhow::anyhow!(messages::util::path_error(&from, e)).into());
        }
    };
    // Keep the lock of the old sandbox until the import is done. Thus no
    // airlock process starts it during the import. Only a missing lock file
    // lets the import go on without a lock. A lock that does not open as a
    // regular file is refused.
    let _old_lock = match old.as_ref().map(|old| old.try_lock("lock")) {
        Some(Err(e)) if e.kind() == std::io::ErrorKind::WouldBlock => {
            return Err(MigrateError::Running);
        }
        Some(Err(e)) => {
            let lock = from.join("lock").map_err(anyhow::Error::from)?;
            return Err(anyhow::anyhow!(messages::util::path_error(&lock, e)).into());
        }
        Some(Ok(lock)) => lock,
        None => None,
    };

    // A different process can have imported the sandbox first.
    let Some(old) = old else {
        return lock_registered(context, host_cwd)
            .await?
            .ok_or_else(|| anyhow::anyhow!(messages::sandboxes::does_not_exist(&from)).into());
    };
    let boxes = &crate::data_dir::create_private_dir(&context.data.sandboxes)?;
    // The registry check and the new entry are one transaction. Thus a
    // rollback removes only an entry that this import made.
    let Some(id) = registry::register_new(&context.db, boxes, host_cwd).await? else {
        return lock_registered(context, host_cwd)
            .await?
            .ok_or_else(|| anyhow::anyhow!(messages::start::SANDBOX_REMOVED_DURING_START).into());
    };
    let lock = match super::lock_box(context, host_cwd, &id).await {
        Ok(Some(lock)) => lock,
        Ok(None) => {
            return Err(anyhow::anyhow!(messages::start::SANDBOX_REMOVED_DURING_START).into());
        }
        Err(e) => {
            registry::unregister(&context.db, &id).await?;
            return Err(e.into());
        }
    };
    let new = match lock.dir().to_path().and_then(PinnedDir::pin) {
        Ok(new) => new,
        Err(e) => {
            let _ = lock.dir().remove_dir_all();
            drop(lock);
            registry::unregister(&context.db, &id).await?;
            return Err(anyhow::Error::from(e).into());
        }
    };

    let mut disk_moved = false;
    let taken = take_entries(context, &old, &new, lock.dir(), &mut disk_moved);
    #[cfg(test)]
    let taken = taken.and_then(|()| run_hook(&AFTER_TAKE));
    let taken = taken.and_then(|()| move_config_files(&airlock, lock.dir()));
    if let Err(e) = taken {
        // Undo all: the old sandbox stays as it was. If the disk cannot go
        // back, the new sandbox is its only copy. Then keep the sandbox and
        // its registry entry.
        if disk_moved && let Err(back) = new.rename_into(DISK_FILE, &old, DISK_FILE) {
            return Err(anyhow::anyhow!(messages::sandboxes::disk_kept(
                new.path().join(DISK_FILE).display(),
                back,
                format!("{e:#}")
            ))
            .into());
        }
        // Unregister after the remove, so that no new sandbox takes the id
        // while the files go.
        let _ = lock.dir().remove_dir_all();
        drop(lock);
        registry::unregister(&context.db, &id).await?;
        return Err(e.into());
    }
    // The import is done. A left `.airlock` directory does no harm: the
    // registry has the project now.
    if let Err(e) = airlock.remove_dir_all() {
        cli::log!(
            "{} {}",
            cli::yellow("!"),
            messages::sandboxes::remove_old_dir_failed(&airlock, e)
        );
    }
    Ok(lock)
}

/// Take the lock of the registered sandbox of the project `host_cwd` (see
/// [`super::lock_box`]).
/// Returns:
///   The held lock, or `None` if the registry has no sandbox of the project.
async fn lock_registered(
    context: &Context,
    host_cwd: &VerifiedPath,
) -> anyhow::Result<Option<SandboxLock>> {
    let project = host_cwd.to_entry_path()?;
    let entry = registry::list(&context.db)
        .await?
        .into_iter()
        .find(|e| e.project == project.as_os_str());
    match entry {
        Some(entry) => super::lock_box(context, host_cwd, &entry.id).await,
        None => Ok(None),
    }
}

/// Take the known files of the old sandbox `old` into the new sandbox
/// directory `new`. A missing or refused file is not taken.
///
/// Args:
///  - `context`: Process context, for the image cache
///  - `old`: Directory of the old sandbox
///  - `new`: Directory of the new sandbox
///  - `new_dir`: Path of `new`
///  - `disk_moved`: Set to `true` when the disk is moved, so that a
///    rollback can move it back
fn take_entries(
    context: &Context,
    old: &PinnedDir,
    new: &PinnedDir,
    new_dir: &VerifiedPath,
    disk_moved: &mut bool,
) -> anyhow::Result<()> {
    // The disk, its records and the CA belong together. Take them only with
    // a valid disk identity, so that the records describe this disk.
    let disk_id = read_old(old, DISK_ID_FILE, DISK_ID_CAP)
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .map(|text| text.trim().to_string())
        .filter(|text| disk::parse_id(text).is_some());
    let disk = disk_id.and_then(|id| Some((id, open_old(old, DISK_FILE)?)));
    let disk_imported = disk.is_some();
    if let Some((id, src)) = disk {
        #[cfg(test)]
        run_hook(&BEFORE_DISK_MOVE)?;
        take_disk(old, new, &src, disk_moved)?;
        new.write_atomic(DISK_ID_FILE, format!("{id}\n").as_bytes(), 0o600)?;
        take_installs(old, new)?;
        if let Some(ca) =
            read_old(old, "ca.json", project::CA_CAP).filter(|bytes| project::is_valid_ca(bytes))
        {
            new.write_atomic("ca.json", &ca, 0o600)?;
        }
    }
    oci::import_image(&context.data, old, new_dir, disk_imported)
}

/// Move the disk `src` (the open disk of `old`) into `new`. Copies it if
/// the two directories are on different file systems.
fn take_disk(
    old: &PinnedDir,
    new: &PinnedDir,
    src: &File,
    disk_moved: &mut bool,
) -> anyhow::Result<()> {
    match old.rename_into(DISK_FILE, new, DISK_FILE) {
        Ok(()) => {
            *disk_moved = true;
            // The guest can swap the entry between the open and the rename.
            // The moved entry must be the file that passed the checks, with
            // no other link.
            let src_meta = src.metadata()?;
            let moved = new.open_regular(DISK_FILE).ok().flatten();
            let same = moved
                .as_ref()
                .and_then(|f| f.metadata().ok())
                .is_some_and(|m| m.dev() == src_meta.dev() && m.ino() == src_meta.ino());
            let Some(moved) = moved.filter(|_| same) else {
                anyhow::bail!(messages::sandboxes::changed_during_import(
                    new.path().join(DISK_FILE).display()
                ));
            };
            moved.set_permissions(std::fs::Permissions::from_mode(0o600))?;
        }
        Err(e) if e.raw_os_error() == Some(libc::EXDEV) => {
            cli::log!(
                "  {} {}",
                cli::bullet(),
                messages::sandboxes::copying_disk(new.path().display())
            );
            clone_from_file(src, new, DISK_FILE)?;
        }
        Err(e) => anyhow::bail!(messages::sandboxes::move_failed(
            old.path().join(DISK_FILE).display(),
            new.path().display(),
            e
        )),
    }
    Ok(())
}

/// Take the install records of `old` into `new`. Records of a run that did
/// not complete become failed. The next install then asks first, because
/// code in the sandbox ran on the disk.
fn take_installs(old: &PinnedDir, new: &PinnedDir) -> anyhow::Result<()> {
    let Some(bytes) = read_old(old, STATE_FILE, STATE_CAP) else {
        return Ok(());
    };
    let ReadState::Ok(mut installs) = state::parse(&bytes) else {
        return Ok(());
    };
    installs.ran_session = true;
    for record in installs.packs.values_mut() {
        if !matches!(
            record.status,
            PackStatus::Installed | PackStatus::Kept { .. }
        ) {
            record.status = PackStatus::Failed;
        }
    }
    state::write(new, &installs)
}

/// Open the file `name` of the old sandbox (see [`PinnedDir::open_regular`]).
/// Returns:
///   The file, or `None` if it is missing or refused.
fn open_old(old: &PinnedDir, name: &str) -> Option<File> {
    old.open_regular(name)
        .inspect_err(|e| tracing::debug!("import: skip {name}: {e}"))
        .ok()
        .flatten()
}

/// Read the file `name` of the old sandbox (see [`PinnedDir::read_single`]).
/// Returns:
///   The content, or `None` if the file is missing, refused or larger than
///   `cap`.
fn read_old(old: &PinnedDir, name: &str, cap: u64) -> Option<Vec<u8>> {
    old.read_single(name, cap)
        .inspect_err(|e| tracing::debug!("import: skip {name}: {e}"))
        .ok()
        .flatten()
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
    move_config_files(airlock, sandbox_dir)?;
    // The check before use refuses a symlink in place of `.airlock`. A
    // symlink at the last name is removed, not followed.
    airlock
        .remove_dir_all()
        .map_err(|e| anyhow::anyhow!(messages::sandboxes::remove_failed(airlock, e)))?;
    Ok(())
}

/// Move the local project config files from the `.airlock` directory
/// `airlock` into `sandbox_dir`. If a move fails, the files that moved go
/// back.
fn move_config_files(airlock: &VerifiedPath, sandbox_dir: &VerifiedPath) -> anyhow::Result<()> {
    let mut moved = Vec::new();
    for name in local_config_names(airlock) {
        let (from, to) = (airlock.join(&name)?, sandbox_dir.join(&name)?);
        if let Err(e) = from.rename_to(&to) {
            for (from, to) in moved.iter().rev() {
                let _ = VerifiedPath::rename_to(to, from);
            }
            anyhow::bail!(messages::sandboxes::move_failed(&from, sandbox_dir, e));
        }
        moved.push((from, to));
    }
    Ok(())
}
