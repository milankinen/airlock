//! Temporary directories for tests.

pub use tempfile::TempDir;

/// A fresh empty directory under the system temp dir, removed on drop.
pub fn temp_dir() -> TempDir {
    tempfile::Builder::new()
        .prefix("airlock-test-")
        .tempdir()
        .unwrap()
}
