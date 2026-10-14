//! Tests for `airlock rm`: what it removes from a project, what it keeps,
//! and how it treats a running sandbox.

use std::path::{Path, PathBuf};

use crate::cache;
use crate::cli::cmd_rm::{RmArgs, run};
use crate::context::Context;
use crate::project::SandboxLock;
use crate::sandboxes::registry;
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::sandboxes::stopped_sandbox_dir;
use crate::test_cfg::{TempDir, block_on_local, temp_dir, test_context};
use crate::vault::{Vault, VaultStorageType};

const FORCE: RmArgs = RmArgs { force: true };

/// The process context of the temp home. Its data directory is `data` in the
/// home, so it shares the image cache with the sandboxes.
fn context(home: &TempDir) -> Context {
    let data_dir = home.path().join("data");
    std::fs::create_dir_all(&data_dir).unwrap();
    test_context(
        &data_dir,
        Vault::for_storage_type(VaultStorageType::Disabled),
    )
}

/// Run `airlock rm` with force in the project `dir`.
fn rm(context: &Context, dir: &Path) -> i32 {
    block_on_local(run(&FORCE, context, &verified_dir(dir)))
}

/// A project in the temp home with a started sandbox. The sandbox image is
/// a hard link to an image in the OCI cache, as `airlock start` makes it.
struct Project {
    dir: PathBuf,
    id: String,
    sandbox: PathBuf,
    image: PathBuf,
}

impl Project {
    /// Make the project `name` with a sandbox disk in the data directory
    /// and a cached image.
    fn started(home: &TempDir, context: &Context, name: &str) -> Self {
        let dir = home.path().join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let dir = std::fs::canonicalize(dir).unwrap();
        let boxes = &context.data.sandboxes;
        let id = block_on_local(registry::find_or_register(
            &context.db,
            boxes,
            &verified_dir(&dir),
        ))
        .unwrap();
        let sandbox = context.data.sandboxes.given().join(&id);
        stopped_sandbox_dir(&sandbox);
        std::fs::write(sandbox.join("disk.img"), b"data").unwrap();
        let image = cache::image_path(&context.data, &format!("sha256:{name}")).unwrap();
        let image = image.to_path().unwrap().to_path_buf();
        std::fs::write(&image, br#"{"schema":"v2","image_layers":[]}"#).unwrap();
        std::fs::hard_link(&image, sandbox.join("image")).unwrap();
        Self {
            dir,
            id,
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

/// Test that rm removes a sandbox in the data directory with its registry
/// entry, its local project config and its unused image. The `.airlock`
/// directory of the project is not the sandbox, so it stays.
///   1. Start a project, and add a local config to the sandbox and a file
///      to `.airlock`
///   2. Run rm with force
///   3. Check that the sandbox, its local config, the registry entry and the
///      image are gone
///   4. Check that the `.airlock` file stays
#[test]
fn rm_of_data_dir_sandbox_removes_it_with_local_config() {
    let home = temp_dir();
    let context = context(&home);
    let project = Project::started(&home, &context, "proj");
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

/// Test that rm refuses to remove the sandbox of a running VM.
///   1. Start a project and hold its sandbox lock, as a running VM does
///   2. Run rm with force and check that it fails
///   3. Check that the disk and the image stay
#[test]
fn rm_of_running_sandbox_is_refused() {
    let home = temp_dir();
    let context = context(&home);
    let project = Project::started(&home, &context, "proj");
    let _running = SandboxLock::acquire_box(
        &verified_dir(&project.dir),
        &context.data.sandboxes,
        &project.id,
    )
    .unwrap();

    assert_eq!(rm(&context, &project.dir), 1);
    assert!(project.sandbox.join("disk.img").is_file());
    assert!(project.image.exists());
}
