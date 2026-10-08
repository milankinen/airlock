//! Tests for the removal of a sandbox from the data directory.

use crate::project::SandboxLock;
use crate::sandboxes::{registry, remove_box};
use crate::test_cfg::sandboxes::data_dir_sandbox;
use crate::test_cfg::{block_on_local, temp_dir, test_context};
use crate::vault::{Vault, VaultStorageType};

/// Test that the removal deletes the sandbox directory and the registry
/// entry, refuses a running sandbox, and refuses a value that is not an id.
///   1. Make a sandbox in the data directory and hold its lock
///   2. Remove it and check that it fails and the sandbox stays
///   3. Release the lock, remove it and check that the directory and the
///      registry entry are gone
///   4. Check that a path in place of an id is refused
#[test]
fn remove_box_deletes_stopped_sandbox_and_refuses_running_or_bad_id() {
    let home = temp_dir();
    let context = test_context(
        home.path(),
        Vault::for_storage_type(VaultStorageType::Disabled),
    );
    let project = std::fs::canonicalize(temp_dir().path()).unwrap();
    let (id, dir) = data_dir_sandbox(&context, &project);
    std::fs::write(dir.join("disk.img"), b"disk").unwrap();

    let running = SandboxLock::acquire_box(&project, &context.boxes_dir(), &id).unwrap();
    assert!(block_on_local(remove_box(&context, &id)).is_err());
    assert!(dir.join("disk.img").is_file());
    drop(running);

    block_on_local(remove_box(&context, &id)).unwrap();
    assert!(!dir.exists());
    assert!(
        block_on_local(registry::list(&context.db))
            .unwrap()
            .is_empty()
    );

    assert!(block_on_local(remove_box(&context, "../../etc")).is_err());
}
