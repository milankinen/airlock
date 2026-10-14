//! Tests for the import of a project sandbox that code in the sandbox could
//! change: planted links and FIFOs, bad files, and the rollback.

use std::collections::BTreeSet;
use std::fs::File;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{FileExt, MetadataExt, symlink};
use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::packs::install::state::{InstallState, PackStatus};
use crate::project::SandboxLock;
use crate::sandboxes::migrate::{AFTER_TAKE, BEFORE_DISK_MOVE};
use crate::sandboxes::{MigrateError, import_legacy, registry};
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::sandboxes::{LEGACY_DISK_ID, cached_image, legacy_ca, legacy_sandbox};
use crate::test_cfg::start::fake_image;
use crate::test_cfg::{TempDir, block_on_local, temp_dir, test_context};
use crate::vault::{Vault, VaultStorageType};

/// Size of the sparse test disk.
const GIB: u64 = 1 << 30;

/// A data directory and a canonical root for projects.
struct Setup {
    _dir: TempDir,
    root: PathBuf,
    context: Context,
}

impl Setup {
    /// Make the data directory in a new temp dir.
    fn new() -> Self {
        let dir = temp_dir();
        let root = std::fs::canonicalize(dir.path()).unwrap();
        let context = test_context(
            &root.join("data"),
            Vault::for_storage_type(VaultStorageType::Disabled),
        );
        Self {
            _dir: dir,
            root,
            context,
        }
    }

    /// Make the canonical project directory `name`.
    fn project(&self, name: &str) -> PathBuf {
        let dir = self.root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Import the project sandbox of `project`.
    fn import(&self, project: &Path) -> Result<SandboxLock, MigrateError> {
        block_on_local(import_legacy(&self.context, &verified_dir(project)))
    }

    /// The number of registered sandboxes.
    fn registered(&self) -> usize {
        block_on_local(registry::list(&self.context.db))
            .unwrap()
            .len()
    }
}

/// The directory of the sandbox that `lock` holds.
fn box_dir(lock: &SandboxLock) -> PathBuf {
    lock.dir().to_path().unwrap().to_path_buf()
}

/// The entry names in `dir`.
fn names(dir: &Path) -> BTreeSet<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// Make a FIFO at `path`.
fn mkfifo(path: &Path) {
    let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
}

/// Make the disk of `sandbox` a sparse 1 GiB file with data at two offsets.
fn sparse_disk(sandbox: &Path) {
    let f = File::create(sandbox.join("disk.img")).unwrap();
    f.set_len(GIB).unwrap();
    f.write_all_at(&[0xab; 5000], 0).unwrap();
    f.write_all_at(&[0xcd; 7000], GIB / 2).unwrap();
}

/// Write an `installs.json` with an installed and an unconfirmed pack, and
/// with no session after the last install.
fn write_installs(sandbox: &Path) {
    let fingerprint = "a".repeat(64);
    let installs = serde_json::json!({
        "version": 1,
        "ran_session": false,
        "packs": {
            "alpha": { "status": "installed", "fingerprint": fingerprint, "at": 1 },
            "beta": { "status": "unconfirmed", "fingerprint": fingerprint, "at": 1 },
        },
    });
    std::fs::write(sandbox.join("installs.json"), installs.to_string()).unwrap();
}

/// Test that the import takes only the known files, as checked regular
/// files, and never follows the links or FIFOs that code in the sandbox can
/// plant. The disk moves without a copy and stays sparse. The old sandbox
/// has no `lock` file, which does not stop the import.
///   1. Make an old sandbox with a sparse disk, records with an unfinished
///      install, an `image` file of a cached image, a symlink to a host
///      file and a FIFO
///   2. Import it
///   3. Check that the new sandbox has only the allowed files, and that
///      `run.json`, the symlink and the FIFO are not taken
///   4. Check that the disk is the same file with the same content and is
///      still sparse
///   5. Check that the image is a link to the cache file, the records are
///      cleaned, and the host file and `.airlock` are as expected
#[test]
fn import_takes_only_checked_files_and_ignores_planted_entries() {
    let setup = Setup::new();
    let project = setup.project("project");
    let old = legacy_sandbox(&project);
    sparse_disk(&old);
    let disk_ino = std::fs::metadata(old.join("disk.img")).unwrap().ino();
    write_installs(&old);
    let (_, image_json) = cached_image(&setup.context, "alpine");
    std::fs::write(old.join("image"), &image_json).unwrap();
    let victim = setup.root.join("victim");
    std::fs::write(&victim, "host secret").unwrap();
    symlink(&victim, old.join("evil")).unwrap();
    mkfifo(&old.join("fifo"));

    let lock = setup.import(&project).unwrap();
    let dir = box_dir(&lock);

    let expected = [
        "ca.json",
        "disk.id",
        "disk.img",
        "image",
        "installs.json",
        "lock",
    ];
    assert_eq!(names(&dir), BTreeSet::from(expected.map(String::from)));
    let disk = std::fs::metadata(dir.join("disk.img")).unwrap();
    assert_eq!(disk.ino(), disk_ino);
    assert_eq!(disk.len(), GIB);
    assert_eq!(disk.mode() & 0o777, 0o600);
    assert!(disk.blocks() * 512 < 64 << 20, "blocks: {}", disk.blocks());
    let f = File::open(dir.join("disk.img")).unwrap();
    let mut data = vec![0u8; 7000];
    f.read_exact_at(&mut data, GIB / 2).unwrap();
    assert!(data.iter().all(|b| *b == 0xcd));
    assert_eq!(
        std::fs::read_to_string(dir.join("disk.id")).unwrap().trim(),
        LEGACY_DISK_ID
    );
    assert_eq!(
        std::fs::read_to_string(dir.join("ca.json")).unwrap(),
        legacy_ca()
    );

    let image = std::fs::metadata(dir.join("image")).unwrap();
    let cache_dir = setup.context.data.images.to_path().unwrap().to_path_buf();
    let cached = std::fs::read_dir(cache_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap();
    assert_eq!(image.ino(), cached.metadata().unwrap().ino());

    let installs: InstallState =
        serde_json::from_slice(&std::fs::read(dir.join("installs.json")).unwrap()).unwrap();
    assert!(installs.ran_session);
    assert_eq!(installs.packs["alpha"].status, PackStatus::Installed);
    assert_eq!(installs.packs["beta"].status, PackStatus::Failed);

    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host secret");
    assert!(!project.join(".airlock").exists());
    assert_eq!(setup.registered(), 1);
}

/// Test that the import refuses hard links, a bad disk identity and a bad
/// CA, so that code in the sandbox cannot make the host take or change a
/// different file. Without its disk, no record, CA or image marker is
/// taken.
///   1. Make an old sandbox with a bad `disk.id`
///   2. Make an old sandbox whose disk and `ca.json` are hard links to host
///      files
///   3. Make an old sandbox with a `ca.json` that is not valid PEM, and an
///      `image` file of an image that is not cached
///   4. Import all three
///   5. Check that the first two new sandboxes have only the lock
///   6. Check that the third has its disk and records but no CA, and the
///      image marker with the old digest
///   7. Check that the host files did not change
#[test]
fn import_refuses_hard_links_bad_disk_id_and_bad_ca() {
    let setup = Setup::new();
    let bad_id = setup.project("bad-id");
    let old = legacy_sandbox(&bad_id);
    std::fs::write(old.join("disk.id"), "not-hex").unwrap();

    let linked = setup.project("linked");
    let old = legacy_sandbox(&linked);
    let (victim_disk, victim_ca) = (setup.root.join("victim.img"), setup.root.join("ca"));
    std::fs::write(&victim_disk, "host disk").unwrap();
    std::fs::write(&victim_ca, legacy_ca()).unwrap();
    for (victim, name) in [(&victim_disk, "disk.img"), (&victim_ca, "ca.json")] {
        std::fs::remove_file(old.join(name)).unwrap();
        std::fs::hard_link(victim, old.join(name)).unwrap();
    }

    let bad_ca = setup.project("bad-ca");
    let old = legacy_sandbox(&bad_ca);
    std::fs::write(old.join("ca.json"), r#"{"cert":"cert","key":"key"}"#).unwrap();
    let uncached = fake_image("busybox");
    let mut image = serde_json::to_value(&uncached).unwrap();
    image["schema"] = "v2".into();
    std::fs::write(old.join("image"), image.to_string()).unwrap();

    for project in [&bad_id, &linked] {
        let lock = setup.import(project).unwrap();
        assert_eq!(names(&box_dir(&lock)), BTreeSet::from(["lock".to_string()]));
    }
    let lock = setup.import(&bad_ca).unwrap();
    let dir = box_dir(&lock);
    let expected = [
        "disk.id",
        "disk.img",
        "image.imported",
        "installs.json",
        "lock",
    ];
    assert_eq!(names(&dir), BTreeSet::from(expected.map(String::from)));
    assert_eq!(
        std::fs::read_to_string(dir.join("image.imported")).unwrap(),
        uncached.image_id
    );
    assert_eq!(std::fs::read_to_string(&victim_disk).unwrap(), "host disk");
    assert_eq!(std::fs::read_to_string(&victim_ca).unwrap(), legacy_ca());
}

/// The number of entries in the sandboxes directory of `setup`.
fn boxes(setup: &Setup) -> usize {
    let dir = setup
        .context
        .data
        .sandboxes
        .to_path()
        .unwrap()
        .to_path_buf();
    if dir.exists() { names(&dir).len() } else { 0 }
}

/// Test that a failed import changes nothing: the moved disk goes back, the
/// new sandbox is removed and not registered, and the old sandbox and the
/// local config stay. Thus the user can start again.
///   1. Make an old sandbox with a local config
///   2. Make the import fail after it took the files
///   3. Check that there is no sandbox and no registry entry
///   4. Check that the old sandbox has its disk and config again
#[test]
fn failed_import_leaves_old_sandbox_and_no_new_sandbox() {
    let setup = Setup::new();
    let project = setup.project("project");
    let old = legacy_sandbox(&project);
    let config = project.join(".airlock/airlock.toml");
    std::fs::write(&config, "[vm]\n").unwrap();

    AFTER_TAKE.set(Some(Box::new(|| anyhow::bail!("test failure"))));
    assert!(setup.import(&project).is_err());

    assert_eq!(setup.registered(), 0);
    assert_eq!(boxes(&setup), 0);
    assert_eq!(
        std::fs::read_to_string(old.join("disk.img")).unwrap(),
        "disk"
    );
    assert_eq!(std::fs::read_to_string(&config).unwrap(), "[vm]\n");
}

/// Test that a disk that code in the sandbox swaps after the open is not
/// taken. A swap to a hard link of a host file would give the host file to
/// the new sandbox.
///   1. Make an old sandbox
///   2. After the open of the disk, move the disk away and put a hard link
///      to a host file at the disk name
///   3. Import and check that it fails, with no sandbox and no registry
///      entry
///   4. Check that the hard link is back in the old sandbox, and that the
///      host file and the real disk did not change
#[test]
fn disk_swapped_after_open_is_refused_and_rolled_back() {
    let setup = Setup::new();
    let project = setup.project("project");
    let old = legacy_sandbox(&project);
    let victim = setup.root.join("victim");
    std::fs::write(&victim, "host file").unwrap();
    let (disk, kept) = (old.join("disk.img"), old.join("disk.real"));
    let link = victim.clone();
    BEFORE_DISK_MOVE.set(Some(Box::new(move || {
        std::fs::rename(&disk, &kept)?;
        std::fs::hard_link(&link, &disk)?;
        Ok(())
    })));

    let Err(err) = setup.import(&project) else {
        panic!("import must fail");
    };
    assert!(
        err.to_string().contains("changed during the import"),
        "{err}"
    );

    assert_eq!(setup.registered(), 0);
    assert_eq!(boxes(&setup), 0);
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");
    assert_eq!(
        std::fs::metadata(old.join("disk.img")).unwrap().ino(),
        std::fs::metadata(&victim).unwrap().ino()
    );
    assert_eq!(
        std::fs::read_to_string(old.join("disk.real")).unwrap(),
        "disk"
    );
}

/// Test that a failed import keeps the new sandbox and its registry entry
/// when the disk cannot go back. The new sandbox then has the only copy of
/// the disk, and a remove would lose it.
///   1. Make an old sandbox
///   2. Make the import fail after it took the files, and put a directory
///      at the old disk name, so that the disk cannot go back
///   3. Import and check that the error names the disk in the new sandbox
///   4. Check that the new sandbox has the disk and stays registered
#[test]
fn failed_import_keeps_new_sandbox_when_disk_cannot_go_back() {
    let setup = Setup::new();
    let project = setup.project("project");
    let old = legacy_sandbox(&project);
    let blocker = old.join("disk.img");
    AFTER_TAKE.set(Some(Box::new(move || {
        std::fs::create_dir_all(blocker.join("x"))?;
        anyhow::bail!("test failure")
    })));

    let Err(err) = setup.import(&project) else {
        panic!("import must fail");
    };

    assert_eq!(setup.registered(), 1);
    let id = &block_on_local(registry::list(&setup.context.db)).unwrap()[0].id;
    let disk = setup
        .context
        .data
        .sandboxes
        .given()
        .join(id)
        .join("disk.img");
    assert!(
        err.to_string().contains(&disk.display().to_string()),
        "{err}"
    );
    assert_eq!(std::fs::read_to_string(&disk).unwrap(), "disk");
}

/// Test that the import refuses an old sandbox whose `lock` is a symlink or
/// a FIFO. Airlock cannot hold such a lock, thus the import cannot know that
/// no process uses the sandbox.
///   1. Make an old sandbox with a `lock` symlink to a host file, and one
///      with a `lock` FIFO
///   2. Import both and check that each import fails and names the lock
///   3. Check that the old sandboxes did not change and the registry is
///      empty
#[test]
fn import_with_lock_symlink_or_fifo_is_refused() {
    let setup = Setup::new();
    let victim = setup.root.join("victim");
    std::fs::write(&victim, "host file").unwrap();
    let linked = setup.project("linked");
    symlink(&victim, legacy_sandbox(&linked).join("lock")).unwrap();
    let fifo = setup.project("fifo");
    mkfifo(&legacy_sandbox(&fifo).join("lock"));

    for project in [&linked, &fifo] {
        let old = project.join(".airlock/sandbox");
        let before = names(&old);
        let Err(err) = setup.import(project) else {
            panic!("import must fail");
        };
        assert!(err.to_string().contains("sandbox/lock"), "{err}");
        assert_eq!(names(&old), before);
        assert_eq!(
            std::fs::read_to_string(old.join("disk.img")).unwrap(),
            "disk"
        );
    }
    assert!(
        std::fs::symlink_metadata(linked.join(".airlock/sandbox/lock"))
            .unwrap()
            .is_symlink()
    );
    assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");
    assert_eq!(setup.registered(), 0);
    assert_eq!(boxes(&setup), 0);
}

/// Test that the import refuses symlinks that code in the sandbox can plant
/// in its project, and that it takes nothing. A followed link could take
/// the sandbox or the config of a different project.
///   1. Make a victim project with a sandbox and a config file
///   2. Make `.airlock` a symlink to the `.airlock` of the victim and check
///      that the import fails
///   3. Make `.airlock/sandbox` a symlink to the victim sandbox and check
///      that the import fails
///   4. Make `airlock.toml` in a real `.airlock` a symlink to the victim
///      config and check that the import fails with the extra file
///   5. Check that the registry is empty, the links stay, and the victim
///      files do not change
#[test]
fn import_with_planted_symlinks_is_refused_and_takes_nothing() {
    let setup = Setup::new();
    let victim = setup.project("victim");
    let victim_sandbox = legacy_sandbox(&victim);
    let victim_config = victim.join(".airlock/airlock.toml");
    std::fs::write(&victim_config, "[vm]\n").unwrap();

    let linked_airlock = setup.project("linked-airlock");
    symlink(victim.join(".airlock"), linked_airlock.join(".airlock")).unwrap();
    assert!(setup.import(&linked_airlock).is_err());

    let linked_sandbox = setup.project("linked-sandbox");
    std::fs::create_dir_all(linked_sandbox.join(".airlock")).unwrap();
    symlink(&victim_sandbox, linked_sandbox.join(".airlock/sandbox")).unwrap();
    assert!(setup.import(&linked_sandbox).is_err());

    let linked_config = setup.project("linked-config");
    let own_sandbox = legacy_sandbox(&linked_config);
    symlink(&victim_config, linked_config.join(".airlock/airlock.toml")).unwrap();
    let Err(err) = setup.import(&linked_config) else {
        panic!("import must fail");
    };
    assert!(
        matches!(&err, MigrateError::ExtraFiles { names } if names == &["airlock.toml"]),
        "{err}"
    );

    assert_eq!(setup.registered(), 0);
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
        legacy_ca()
    );
    assert!(victim_sandbox.join("disk.img").is_file());
}
