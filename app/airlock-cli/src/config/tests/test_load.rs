use std::path::PathBuf;

use crate::config::load_config::load_first;

/// Unique base path under the system temp dir (no `tempfile` dep in this
/// crate; mirrors the manual temp-dir helper used in `oci::gc` tests).
fn temp_base() -> PathBuf {
    std::env::temp_dir().join(format!(
        "airlock-loadcfg-test-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

#[test]
fn missing_file_yields_none() {
    // No file at any extension → None, and crucially no error.
    let base = temp_base();
    assert!(load_first(&base).unwrap().is_none());
}

#[test]
fn unreadable_file_is_error_not_skipped() {
    // A config that EXISTS but can't be read must fail closed rather than
    // fall through to defaults. Reading a directory named `<base>.toml`
    // yields a non-`NotFound` error on every user (root included), so this
    // is a reliable stand-in for a permission/IO failure.
    let base = temp_base();
    let dir = PathBuf::from(format!("{}.toml", base.display()));
    std::fs::create_dir_all(&dir).unwrap();

    let result = load_first(&base);
    std::fs::remove_dir(&dir).ok();

    assert!(
        result.is_err(),
        "an existing-but-unreadable config must fail closed, got {result:?}"
    );
}
