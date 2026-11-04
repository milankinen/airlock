//! Tests for the removal of a new sandbox after a failed first start.

use crate::sandbox::first_start::FirstStart;
use crate::sandbox::migrate::AFTER_CONFIG_MOVE;
use crate::sandbox::registry;
use crate::sandbox::start::Exit;
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::sandboxes::lock_id;
use crate::test_cfg::start::{locate_sandbox, lock_sandbox};
use crate::test_cfg::{block_on_local, temp_dir, test_context};
use crate::vault::{Vault, VaultStorageType};

/// Test that a failed first start removes the new sandbox, and that a
/// sandbox with a disk or with a moved local config stays. The next start
/// must see a new sandbox, but a failed start must never delete user data.
///   1. Register and lock a new sandbox, as the `start` client does
///   2. Remove it as after a failed first start, and check that the
///      directory and the registry entry are gone
///   3. Lock the sandbox again, add a disk, and check that the removal
///      keeps it
///   4. In a second project with a local config in `.airlock`, run the
///      location steps, and check that the removal keeps the sandbox with
///      the moved config
#[test]
fn failed_first_start_removes_only_new_sandbox_without_user_files() {
    let home = temp_dir();
    let context = test_context(
        home.path(),
        Vault::for_storage_type(VaultStorageType::Disabled),
    );
    let project_tmp = temp_dir();
    let project = verified_dir(&std::fs::canonicalize(project_tmp.path()).unwrap());
    let registered = || block_on_local(registry::list(&context.db)).unwrap().len();

    let lock = block_on_local(lock_sandbox(&context, &project)).unwrap();
    let dir = lock.dir().to_path().unwrap().to_path_buf();
    let first = FirstStart::check(&lock);
    assert!(first.is_new());
    block_on_local(first.remove(&context.db, &lock, &lock_id(&lock)));
    drop(lock);
    assert!(!dir.exists());
    assert_eq!(registered(), 0);

    let lock = block_on_local(lock_sandbox(&context, &project)).unwrap();
    std::fs::write(lock.dir().to_path().unwrap().join("disk.img"), b"disk").unwrap();
    let first = FirstStart::check(&lock);
    assert!(!first.is_new());
    block_on_local(first.remove(&context.db, &lock, &lock_id(&lock)));
    assert!(lock.dir().to_path().unwrap().join("disk.img").is_file());
    assert_eq!(registered(), 1);

    let other_tmp = temp_dir();
    let other = std::fs::canonicalize(other_tmp.path()).unwrap();
    std::fs::create_dir(other.join(".airlock")).unwrap();
    std::fs::write(other.join(".airlock/airlock.toml"), "[vm]\n").unwrap();
    let lock = block_on_local(lock_sandbox(&context, &verified_dir(&other))).unwrap();
    let mut first = FirstStart::check(&lock);
    crate::sandbox::start::location::fill_new_sandbox(&context, &lock, &mut first, false, false)
        .unwrap();
    block_on_local(first.remove(&context.db, &lock, &lock_id(&lock)));
    assert!(lock.dir().to_path().unwrap().join("airlock.toml").is_file());
    assert_eq!(registered(), 2);
}

/// Test that a local config move that fails after the files moved keeps
/// the sandbox. The sandbox then has the only copy of the config, and the
/// cleanup of a failed first start must not delete it.
///   1. Write a local project config in `.airlock` of a new project
///   2. Make the move fail after the files moved, before `.airlock` goes
///   3. Run the location steps and check exit code 1
///   4. Check that the sandbox stays registered with the config in it
#[test]
fn failed_local_config_move_after_files_moved_keeps_sandbox() {
    let home = temp_dir();
    let context = test_context(
        home.path(),
        Vault::for_storage_type(VaultStorageType::Disabled),
    );
    let project_tmp = temp_dir();
    let project = std::fs::canonicalize(project_tmp.path()).unwrap();
    std::fs::create_dir(project.join(".airlock")).unwrap();
    std::fs::write(project.join(".airlock/airlock.toml"), "[vm]\n").unwrap();
    AFTER_CONFIG_MOVE.set(Some(Box::new(|| anyhow::bail!("remove failed"))));

    let result = block_on_local(locate_sandbox(
        &context,
        &verified_dir(&project),
        false,
        false,
    ));

    assert!(matches!(result, Err(Exit::Code(1))));
    let entries = block_on_local(registry::list(&context.db)).unwrap();
    assert_eq!(entries.len(), 1);
    let dir = context.data.sandboxes.given().join(&entries[0].id);
    assert_eq!(
        std::fs::read_to_string(dir.join("airlock.toml")).unwrap(),
        "[vm]\n"
    );
}
