//! Tests for the move of a project sandbox with planted symlinks.

use std::os::unix::fs::symlink;
use std::path::Path;

use crate::sandboxes::{MigrateError, migrate, registry};
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::sandboxes::{LEGACY_CA, legacy_sandbox};
use crate::test_cfg::{block_on_local, temp_dir, test_context};
use crate::vault::{Vault, VaultStorageType};

/// Make the canonical directory `name` in `root`.
fn project(root: &Path, name: &str) -> std::path::PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Test that the move refuses symlinks that code in the sandbox can plant
/// in its project, and that it moves nothing. A followed link could move
/// the sandbox or the config of a different project into this sandbox.
///   1. Make a victim project with a sandbox and a config file
///   2. Make `.airlock` a symlink to the `.airlock` of the victim and check
///      that the move fails
///   3. Make `.airlock/sandbox` a symlink to the victim sandbox and check
///      that the move fails
///   4. Make `airlock.toml` in a real `.airlock` a symlink to the victim
///      config and check that the move fails with the extra file
///   5. Check that the registry is empty, the links stay, and the victim
///      files do not change
#[test]
fn migrate_with_planted_symlinks_is_refused_and_moves_nothing() {
    let home = temp_dir();
    let root = std::fs::canonicalize(home.path()).unwrap();
    let context = test_context(
        &root.join("data"),
        Vault::for_storage_type(VaultStorageType::Disabled),
    );
    let victim = project(&root, "victim");
    let victim_sandbox = legacy_sandbox(&victim);
    let victim_config = victim.join(".airlock/airlock.toml");
    std::fs::write(&victim_config, "[vm]\n").unwrap();

    let linked_airlock = project(&root, "linked-airlock");
    symlink(victim.join(".airlock"), linked_airlock.join(".airlock")).unwrap();
    assert!(block_on_local(migrate(&context, &verified_dir(&linked_airlock))).is_err());

    let linked_sandbox = project(&root, "linked-sandbox");
    std::fs::create_dir_all(linked_sandbox.join(".airlock")).unwrap();
    symlink(&victim_sandbox, linked_sandbox.join(".airlock/sandbox")).unwrap();
    assert!(block_on_local(migrate(&context, &verified_dir(&linked_sandbox))).is_err());

    let linked_config = project(&root, "linked-config");
    let own_sandbox = legacy_sandbox(&linked_config);
    symlink(&victim_config, linked_config.join(".airlock/airlock.toml")).unwrap();
    let err = block_on_local(migrate(&context, &verified_dir(&linked_config))).unwrap_err();
    assert!(
        matches!(&err, MigrateError::ExtraFiles { names } if names == &["airlock.toml"]),
        "{err}"
    );

    assert!(
        block_on_local(registry::list(&context.db))
            .unwrap()
            .is_empty()
    );
    assert!(own_sandbox.join("disk.img").is_file());
    for link in [
        linked_airlock.join(".airlock"),
        linked_sandbox.join(".airlock/sandbox"),
        linked_config.join(".airlock/airlock.toml"),
    ] {
        assert!(link.symlink_metadata().unwrap().is_symlink(), "{link:?}");
    }
    assert_eq!(std::fs::read_to_string(&victim_config).unwrap(), "[vm]\n");
    assert_eq!(
        std::fs::read_to_string(victim_sandbox.join("ca.json")).unwrap(),
        LEGACY_CA
    );
    assert!(victim_sandbox.join("disk.img").is_file());
}
