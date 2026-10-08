use std::ffi::OsString;
use std::path::Path;
use std::sync::MutexGuard;

use airlock_test_utils::{TempDir, temp_dir};

use crate::cache::HOME_LOCK;

/// `HOME` pointed at a fresh temp dir (so the global cache under
/// `~/.cache/airlock` is the test's own) while the value lives. Holds the
/// crate-wide `HOME` lock and restores the old `HOME` on drop.
pub struct TempHome {
    dir: TempDir,
    old: Option<OsString>,
    _lock: MutexGuard<'static, ()>,
}

impl TempHome {
    pub fn new() -> Self {
        let lock = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let dir = temp_dir();
        let old = std::env::var_os("HOME");
        // SAFETY: every test that reads or writes `HOME` holds `HOME_LOCK`.
        unsafe { std::env::set_var("HOME", dir.path()) };
        Self {
            dir,
            old,
            _lock: lock,
        }
    }

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
