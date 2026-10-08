//! Tests for `airlock rm`: what it removes from a project, what it keeps,
//! and how it treats a running sandbox and symlinks.

use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

use crate::cache;
use crate::cli::cmd_rm::{RmArgs, run};
use crate::context::Context;
use crate::project::SandboxLock;
use crate::sandboxes::registry;
use crate::test_cfg::home::TempHome;
use crate::test_cfg::{block_on_local, test_context};
use crate::vault::{Vault, VaultStorageType};

const FORCE: RmArgs = RmArgs { force: true };

/// The process context of the temp home. Its data directory is the default
/// one, so it shares the image cache with the sandboxes.
fn context(home: &TempHome) -> Context {
    test_context(
        &home.data_dir(),
        Vault::for_storage_type(VaultStorageType::Disabled),
    )
}

/// Run `airlock rm` with force in the project `dir`.
fn rm(context: &Context, dir: &Path) -> i32 {
    block_on_local(run(&FORCE, context, dir))
}

/// A project in the temp home with a started sandbox. The sandbox image is
/// a hard link to an image in the OCI cache, as `airlock start` makes it.
struct Project {
    dir: PathBuf,
    sandbox: PathBuf,
    image: PathBuf,
}

impl Project {
    /// Make the project `name` with a sandbox disk in the project and a
    /// cached image.
    fn started(home: &TempHome, name: &str) -> Self {
        let dir = Self::make_dir(home, name);
        // The lock makes `.airlock/sandbox`. Release it at once, so that
        // the sandbox is not running.
        let sandbox = SandboxLock::acquire(&dir).unwrap().dir().to_path_buf();
        Self::finish(dir, sandbox, name)
    }

    /// Make the project `name` with a sandbox disk in the data directory
    /// and a cached image.
    fn started_in_data_dir(home: &TempHome, context: &Context, name: &str) -> Self {
        let dir = Self::make_dir(home, name);
        let boxes = context.boxes_dir();
        let id = block_on_local(registry::find_or_register(&context.db, &boxes, &dir)).unwrap();
        let sandbox = SandboxLock::acquire_box(&dir, &boxes, &id)
            .unwrap()
            .dir()
            .to_path_buf();
        Self::finish(dir, sandbox, name)
    }

    /// Make the canonical project directory `name` in the temp home.
    fn make_dir(home: &TempHome, name: &str) -> PathBuf {
        let dir = home.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::canonicalize(dir).unwrap()
    }

    /// Add a disk and a cached image to the sandbox `sandbox` of `dir`.
    fn finish(dir: PathBuf, sandbox: PathBuf, name: &str) -> Self {
        std::fs::write(sandbox.join("disk.img"), b"data").unwrap();
        let image = cache::image_path(&format!("sha256:{name}")).unwrap();
        std::fs::write(&image, br#"{"schema":"v2","image_layers":[]}"#).unwrap();
        std::fs::hard_link(&image, sandbox.join("image")).unwrap();
        Self {
            dir,
            sandbox,
            image,
        }
    }

    /// Return the `.airlock` directory of the project.
    fn airlock(&self) -> PathBuf {
        self.dir.join(".airlock")
    }
}

/// Return true if `path` exists. A broken symlink also counts.
fn exists(path: &Path) -> bool {
    std::fs::symlink_metadata(path).is_ok()
}

/// Test that rm removes all of `.airlock` and the cached image when only
/// airlock files are in it. The project directory itself must stay.
///   1. Start a project and add a local config and a `.gitignore`
///   2. Run rm with force
///   3. Check that `.airlock` and the image are gone and the project stays
#[test]
fn rm_of_project_removes_its_airlock_dir_and_unused_image() {
    let home = TempHome::new();
    let context = context(&home);
    let project = Project::started(&home, "proj");
    std::fs::write(project.airlock().join("airlock.toml"), "").unwrap();
    std::fs::write(project.airlock().join(".gitignore"), "*\n").unwrap();

    assert_eq!(rm(&context, &project.dir), 0);

    assert!(!exists(&project.airlock()));
    assert!(!project.image.exists());
    assert!(project.dir.is_dir());
}

/// Test that rm removes a sandbox in the data directory with its registry
/// entry, its local project config and its unused image. The `.airlock`
/// directory of the project is not the sandbox, so it stays.
///   1. Start a project with its sandbox in the data directory, and add a
///      local config to the sandbox and a file to `.airlock`
///   2. Run rm with force
///   3. Check that the sandbox, its local config, the registry entry and the
///      image are gone
///   4. Check that the `.airlock` file stays
#[test]
fn rm_of_data_dir_sandbox_removes_it_with_local_config() {
    let home = TempHome::new();
    let context = context(&home);
    let project = Project::started_in_data_dir(&home, &context, "proj");
    std::fs::write(project.sandbox.join("airlock.toml"), "").unwrap();
    std::fs::create_dir_all(project.airlock()).unwrap();
    std::fs::write(project.airlock().join(".init-done"), "").unwrap();

    assert_eq!(rm(&context, &project.dir), 0);

    assert!(!exists(&project.sandbox));
    assert!(
        block_on_local(registry::list(&context.db))
            .unwrap()
            .is_empty()
    );
    assert!(!project.image.exists());
    assert!(project.airlock().join(".init-done").is_file());
}

/// Test that rm keeps `.airlock` when it holds user data, and removes only
/// the sandbox. Old versions kept settings, vaults and agent data there.
///   1. For each user file or directory, start a project and add it
///   2. Run rm with force
///   3. Check that the user data stays, and the sandbox and image are gone
#[test]
fn rm_of_airlock_dir_with_user_files_removes_only_sandbox() {
    let home = TempHome::new();
    let context = context(&home);
    let markers = [
        ("settings.yaml", false),
        ("config.json", false),
        ("vault.default.json", false),
        ("vault.default.enc.json", false),
        (crate::db::DIR, true),
        ("claude", true),
        ("codex", true),
        ("agents", true),
    ];

    for (marker, is_dir) in markers {
        let project = Project::started(&home, &format!("proj-{marker}"));
        let path = project.airlock().join(marker);
        if is_dir {
            std::fs::create_dir_all(&path).unwrap();
        } else {
            std::fs::write(&path, "{}").unwrap();
        }

        assert_eq!(rm(&context, &project.dir), 0, "{marker}");

        assert!(exists(&path), "{marker}");
        assert!(!exists(&project.airlock().join("sandbox")), "{marker}");
        assert!(!project.image.exists(), "{marker}");
    }
}

/// Test that rm refuses to remove the sandbox of a running VM, in the
/// project and in the data directory.
///   1. Start a project of each kind and hold its sandbox lock, as a running
///      VM does
///   2. Run rm with force and check that it fails
///   3. Check that the disk and the image stay
#[test]
fn rm_of_running_sandbox_is_refused() {
    let home = TempHome::new();
    let context = context(&home);
    let in_project = Project::started(&home, "proj");
    let in_data_dir = Project::started_in_data_dir(&home, &context, "boxed");
    let _running = SandboxLock::acquire(&in_project.dir).unwrap();
    let id = block_on_local(registry::list(&context.db)).unwrap()[0]
        .id
        .clone();
    let _running_box =
        SandboxLock::acquire_box(&in_data_dir.dir, &context.boxes_dir(), &id).unwrap();

    for project in [&in_project, &in_data_dir] {
        assert_eq!(rm(&context, &project.dir), 1);
        assert!(project.sandbox.join("disk.img").is_file());
        assert!(project.image.exists());
    }
}

/// Test that rm removes a symlinked `.airlock` or `.airlock/sandbox` as a
/// link only. A planted link must not let rm delete the data of a different
/// project.
///   1. Start a victim project with a user file
///   2. Run rm in a project whose `.airlock` links to the victim, and check
///      that only the link is gone
///   3. Run rm in projects whose sandbox links to the victim, with and
///      without user files, and check that `.airlock` stays only with them
///   4. Check that the victim files and image stay
#[test]
fn rm_removes_symlinked_airlock_dir_or_sandbox_but_not_their_targets() {
    let home = TempHome::new();
    let context = context(&home);
    let victim = Project::started(&home, "victim");
    let victim_disk = victim.airlock().join("sandbox/disk.img");
    std::fs::write(victim.airlock().join("settings.yaml"), "").unwrap();

    let linked_airlock = home.path().join("linked-airlock");
    std::fs::create_dir_all(&linked_airlock).unwrap();
    symlink(victim.airlock(), linked_airlock.join(".airlock")).unwrap();
    assert_eq!(rm(&context, &linked_airlock), 0);
    assert!(!exists(&linked_airlock.join(".airlock")));

    for user_dir in [true, false] {
        let project = home.path().join(format!("linked-sandbox-{user_dir}"));
        std::fs::create_dir_all(project.join(".airlock")).unwrap();
        if user_dir {
            std::fs::write(project.join(".airlock/settings.yaml"), "").unwrap();
        }
        symlink(
            victim.airlock().join("sandbox"),
            project.join(".airlock/sandbox"),
        )
        .unwrap();
        assert_eq!(rm(&context, &project), 0);
        assert!(!exists(&project.join(".airlock/sandbox")));
        assert_eq!(exists(&project.join(".airlock")), user_dir);
    }

    assert!(victim.airlock().join("settings.yaml").is_file());
    assert!(victim_disk.is_file());
    assert!(victim.image.exists());
}
