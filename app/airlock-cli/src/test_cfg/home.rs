//! A temporary `HOME` for tests that use the global airlock cache.

use std::ffi::OsString;
use std::path::Path;
use std::sync::MutexGuard;

use airlock_test_utils::{TempDir, temp_dir};

use crate::cache::HOME_LOCK;

/// Sets `HOME` to a new temporary directory while the value exists. Thus
/// the global cache under `~/.cache/airlock` belongs to the test. Holds the
/// crate-wide `HOME` lock, and sets the old `HOME` again on drop.
pub struct TempHome {
    dir: TempDir,
    old: Option<OsString>,
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
        // SAFETY: each test that reads or writes `HOME` holds `HOME_LOCK`.
        unsafe { std::env::set_var("HOME", dir.path()) };
        Self {
            dir,
            old,
            _lock: lock,
        }
    }

    /// The temporary home directory.
    pub fn path(&self) -> &Path {
        self.dir.path()
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
        }
    }
}
