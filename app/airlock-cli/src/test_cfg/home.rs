//! A temporary `HOME` for tests that depend on the home directory.

use std::ffi::OsString;
use std::path::Path;
use std::sync::MutexGuard;

use airlock_test_utils::{TempDir, temp_dir};

use crate::cache::HOME_LOCK;

/// Sets `HOME` to a new temporary directory while the value exists, and
/// removes `XDG_DATA_HOME`. Thus the default airlock data directory belongs
/// to the test. Holds the crate-wide `HOME` lock, and sets the old values
/// again on drop.
pub struct TempHome {
    dir: TempDir,
    old: Option<OsString>,
    old_xdg: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl TempHome {
    /// Take the `HOME` lock and set `HOME` to a new temporary directory.
    pub fn new() -> Self {
        let lock = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = temp_dir();
        let old = std::env::var_os("HOME");
        let old_xdg = std::env::var_os("XDG_DATA_HOME");
        // SAFETY: each test that reads or writes `HOME` holds `HOME_LOCK`.
        unsafe {
            std::env::set_var("HOME", dir.path());
            std::env::remove_var("XDG_DATA_HOME");
        }
        Self {
            dir,
            old,
            old_xdg,
            _lock: lock,
        }
    }

    /// The temporary home directory.
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// The default airlock data directory in the temporary home.
    pub fn data_dir(&self) -> std::path::PathBuf {
        let dir = crate::cache::default_data_dir().unwrap();
        assert!(dir.starts_with(self.path()), "{}", dir.display());
        dir
    }
}

impl Default for TempHome {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        // SAFETY: `HOME_LOCK` is still held.
        unsafe {
            match self.old.take() {
                Some(old) => std::env::set_var("HOME", old),
                None => std::env::remove_var("HOME"),
            }
            if let Some(old) = self.old_xdg.take() {
                std::env::set_var("XDG_DATA_HOME", old);
            }
        }
    }
}
