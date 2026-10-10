//! Helpers that make sandboxes of both kinds: in the data directory, and in
//! the project directory as older airlock versions made them.

use std::path::{Path, PathBuf};

use airlock_test_utils::block_on_local;

use crate::context::Context;
use crate::sandboxes::registry;

/// The `ca.json` of a legacy sandbox.
pub const LEGACY_CA: &str = r#"{"cert":"legacy-cert","key":"legacy-key"}"#;

/// Make a stopped sandbox in `project/.airlock/sandbox` as an older airlock
/// made it: the record files and a disk image.
/// Returns:
///   The sandbox directory.
pub fn legacy_sandbox(project: &Path) -> PathBuf {
    let dir = project.join(".airlock/sandbox");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ca.json"), LEGACY_CA).unwrap();
    std::fs::write(
        dir.join("run.json"),
        r#"{"last_run":7,"guest_cwd":"/work"}"#,
    )
    .unwrap();
    std::fs::write(dir.join("installs.json"), r#"{"version":1}"#).unwrap();
    std::fs::write(dir.join("disk.img"), b"disk").unwrap();
    dir
}

/// Make the directory `dir` of a stopped sandbox, with the lock file that a
/// run leaves.
///
/// Takes no lock. A lock that the test takes and releases can stay held for
/// a short time, because a `fork` in a parallel test copies the open lock
/// file.
pub fn stopped_sandbox_dir(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("lock"), b"").unwrap();
}

/// Make a stopped sandbox of the project `project` in the data directory of
/// `context`.
/// Returns:
///   The registry id and the sandbox directory.
pub fn data_dir_sandbox(context: &Context, project: &Path) -> (String, PathBuf) {
    let boxes = context.boxes_dir();
    let id = block_on_local(registry::find_or_register(&context.db, &boxes, project)).unwrap();
    let dir = boxes.join(&id);
    stopped_sandbox_dir(&dir);
    (id, dir)
}
