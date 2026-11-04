//! First start of a sandbox.
//!
//! A sandbox directory with only the lock files and the log is new. When
//! the first start of such a sandbox fails or the user aborts it before
//! the sandbox has a disk, the sandbox goes away again: its registry entry
//! and its directory. Thus the next start sees a new sandbox. When the
//! start moves files of the user into the sandbox directory, the sandbox
//! stays. A sandbox with a disk also stays: the next start continues the
//! installs on it.

use super::lock::{LockKind, SandboxLock};
use super::migrate::MigrateError;
use super::registry;
use crate::db::Db;

/// Entries of a new sandbox directory: the lock files and the log.
const NEW_ENTRIES: [&str; 4] = [
    LockKind::Run.file_name(),
    LockKind::Startup.file_name(),
    // The lock of older airlock versions.
    "lock",
    crate::cli::logging::LOG_FILE,
];

/// The state of the first start of a sandbox.
#[derive(Debug, Default)]
pub struct FirstStart {
    /// The sandbox is new and has no files of the user.
    new: bool,
    /// The sandbox files come from a failed import. They are not a disk
    /// of the user.
    failed_import: bool,
}

impl FirstStart {
    /// Check the sandbox that `lock` holds.
    /// Returns:
    ///   A new first start if the sandbox directory has only the lock files
    ///   and the log. Else a start that never removes the sandbox.
    pub fn check(lock: &SandboxLock) -> Self {
        Self {
            new: only_new_entries(lock),
            failed_import: false,
        }
    }

    /// Return true if the sandbox is new and has no files of the user.
    pub fn is_new(&self) -> bool {
        self.new
    }

    /// Keep the sandbox also if the start fails: it has files of the user
    /// now.
    pub fn keep(&mut self) {
        self.new = false;
    }

    /// Keep the sandbox if its directory has other entries than a new
    /// sandbox now. Use this after a failed step that can have moved files
    /// of the user into the directory.
    pub fn keep_if_changed(&mut self, lock: &SandboxLock) {
        if !only_new_entries(lock) {
            self.keep();
        }
    }

    /// Keep the sandbox after an import of an older project sandbox that
    /// worked, or that left the only copy of the disk in the sandbox.
    pub fn after_import(&mut self, result: &Result<(), MigrateError>) {
        match result {
            Ok(()) | Err(MigrateError::DiskKept(_)) => self.keep(),
            Err(_) => self.failed_import = true,
        }
    }

    /// Remove a new sandbox after a failed first start: first its registry
    /// entry, then its directory. Does nothing for a sandbox that
    /// is not new, or that has a disk or install records now (except from a
    /// failed import). A failure goes only to the log.
    /// Args:
    ///  - `db`: The airlock database with the registry
    ///  - `lock`: The held locks of the sandbox. Keep them until this
    ///    returns, so that no other start takes the sandbox meanwhile
    ///  - `id`: Registry id of the sandbox
    pub async fn remove(self, db: &Db, lock: &SandboxLock, id: &str) {
        if !self.new || (!self.failed_import && super::lookup::dir_has_content(lock.dir())) {
            return;
        }
        tracing::info!("first start failed, remove sandbox {id}");
        // Unregister first. A different start that waits for the locks then
        // fails its registry check under the locks, and does not use the
        // directory that goes. A new id never names an existing directory.
        if let Err(e) = registry::unregister(db, id).await {
            tracing::warn!("unregister sandbox {id}: {e:#}");
            return;
        }
        // A symlink at the directory is removed, not followed.
        if let Err(e) = lock.dir().remove_dir_all()
            && e.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!("remove sandbox dir {}: {e}", lock.dir());
        }
    }
}

/// Check if the directory of `lock` has only the entries of a new sandbox.
/// An entry that does not read counts as another entry.
fn only_new_entries(lock: &SandboxLock) -> bool {
    lock.dir().read_dir().is_ok_and(|mut entries| {
        entries.all(|entry| {
            entry.is_ok_and(|entry| {
                let name = entry.file_name();
                NEW_ENTRIES.iter().any(|known| name == *known)
            })
        })
    })
}
