//! Tests of the sandbox location step of `airlock start`: where a new
//! sandbox goes, and the import of a sandbox of an older version from the
//! project directory into the data directory.

use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::project::{self, SandboxLock};
use crate::sandboxes::{self, import_legacy, registry};
use crate::start::Exit;
use crate::start::location::lock_sandbox;
use crate::test_cfg::packs::test_packs;
use crate::test_cfg::sandboxes::{cached_image, legacy_ca, legacy_sandbox};
use crate::test_cfg::start::StartProject;
use crate::test_cfg::{TempDir, block_on_local, eventually, temp_dir, test_context};
use crate::util::VerifiedPath;
use crate::vault::{Vault, VaultStorageType};

/// A config with one pack that has a setup script.
const ONE: &str = "[packs]\nalpha = { version = 1 }\n";

/// A home with the user settings `settings`, its context and a canonical
/// project directory.
struct Home {
    _dir: TempDir,
    _project: TempDir,
    context: Context,
    project: PathBuf,
}

impl Home {
    /// Make the home with the settings file `settings` (TOML).
    fn new(settings: &str) -> Self {
        let home = temp_dir();
        std::fs::write(home.path().join("settings.toml"), settings).unwrap();
        let context = test_context(
            home.path(),
            Vault::for_storage_type(VaultStorageType::Disabled),
        );
        let project_dir = temp_dir();
        let project = std::fs::canonicalize(project_dir.path()).unwrap();
        Self {
            _dir: home,
            _project: project_dir,
            context,
            project,
        }
    }

    /// Run the location step without a terminal. `yes` is `--yes`. Panics
    /// if the step fails (see [`Self::try_lock`]).
    /// Returns:
    ///   The sandbox directory. The lock is released.
    fn lock(&self, yes: bool) -> PathBuf {
        self.try_lock(yes).unwrap_or_else(|e| panic!("{e:?}"))
    }

    /// Run the location step without a terminal. `yes` is `--yes`.
    ///
    /// The lock of a previous step can stay held for a short time after it
    /// drops: a process that a parallel test starts has a copy of the lock
    /// descriptor until it execs. Thus the step waits until the lock is
    /// free, and tries again if it finds the lock held (as
    /// [`StartProject::start`] does).
    /// Returns:
    ///   The sandbox directory, or the exit of the step. The lock is
    ///   released.
    fn try_lock(&self, yes: bool) -> Result<PathBuf, Exit> {
        let project = crate::test_cfg::oci::verified_dir(&self.project);
        eventually("sandbox lock is free", || {
            // Wait until the lock of the old or the registered sandbox is
            // free.
            let old = VerifiedPath::new_follow(self.project_sandbox(), None).unwrap();
            let found = block_on_local(sandboxes::resolve_sandbox(
                &self.context,
                &project,
                false,
                false,
            ))
            .unwrap();
            if project::is_running(&old) || found.is_some_and(|f| project::is_running(&f.dir)) {
                return None;
            }
            match block_on_local(lock_sandbox(&self.context, &project, false, yes)) {
                Ok(lock) => Some(Ok(lock.dir().to_path().unwrap().to_path_buf())),
                Err(e) if format!("{e:?}").contains("another airlock instance") => None,
                Err(e) => Some(Err(e)),
            }
        })
    }

    /// The registered sandboxes.
    fn registered(&self) -> Vec<registry::Entry> {
        block_on_local(registry::list(&self.context.db)).unwrap()
    }

    /// The project sandbox directory.
    fn project_sandbox(&self) -> PathBuf {
        self.project.join(".airlock/sandbox")
    }
}

/// The content of the file `name` in the sandbox directory `dir`.
fn read(dir: &Path, name: &str) -> String {
    std::fs::read_to_string(dir.join(name)).unwrap()
}

/// Test that a new sandbox goes to the data directory with its records,
/// and that the project directory gets no sandbox data.
///   1. Start a project with one pack
///   2. Check that the sandbox is registered and is in the data directory
///   3. Check that the project has no `.airlock` directory
///   4. Check that the sandbox has the CA, the run data and the install
///      records
///   5. Start again and check that the same sandbox is used
#[test]
fn new_sandbox_goes_to_data_dir_and_project_gets_no_sandbox_data() {
    let project = StartProject::new(test_packs());
    project.start(ONE, false).unwrap();

    let found = project.sandbox().unwrap();
    let id = &found.id;
    let dir = found.dir.to_path().unwrap();
    let boxes = &project.context().data.sandboxes;
    assert_eq!(found.dir, boxes.join(id).unwrap());
    assert!(!project.project_dir().join(".airlock").exists());
    for name in ["ca.json", "run.json", "installs.json"] {
        assert!(dir.join(name).is_file(), "{name}");
    }

    project.start(ONE, false).unwrap();
    let entries = block_on_local(registry::list(&project.context().db)).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(&entries[0].id, id);
}

/// Test that `--yes` imports an older project sandbox into the data
/// directory with its disk, records and local project config, and removes
/// the `.airlock` directory of the project.
///   1. Make a sandbox with record files and a disk in the project, and a
///      local project config, a `.gitignore` and a log next to it
///   2. Run the location step with `--yes`
///   3. Check that the sandbox is registered and is in the data directory
///   4. Check that the disk, the records and the local config moved, and
///      that `run.json` is dropped
///   5. Check that the sandbox has the image marker, because the old
///      sandbox had no image
///   6. Check that the `.airlock` directory of the project is gone
#[test]
fn yes_moves_project_sandbox_and_local_config_and_removes_airlock_dir() {
    let home = Home::new("");
    legacy_sandbox(&home.project);
    let airlock = home.project.join(".airlock");
    for (name, content) in [
        ("airlock.toml", "[vm]\n"),
        (".gitignore", "*\n"),
        ("airlock.log", "log"),
    ] {
        std::fs::write(airlock.join(name), content).unwrap();
    }

    let dir = home.lock(true);

    let entries = block_on_local(registry::list(&home.context.db)).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0].project, home.project.as_os_str());
    assert_eq!(
        dir,
        home.context
            .data
            .sandboxes
            .to_path()
            .unwrap()
            .join(&entries[0].id)
    );
    assert_eq!(read(&dir, "disk.img"), "disk");
    assert_eq!(read(&dir, "ca.json"), legacy_ca());
    assert!(dir.join("installs.json").is_file());
    assert!(!dir.join("run.json").exists());
    assert!(!dir.join("image").exists());
    assert_eq!(read(&dir, "image.imported"), "");
    assert_eq!(read(&dir, "airlock.toml"), "[vm]\n");
    assert!(!airlock.exists());
}

/// Test that a move is refused, and changes nothing, when the `.airlock`
/// directory of the project holds files that airlock does not know.
///   1. Make a project sandbox and an unknown file in `.airlock`
///   2. Run the move and check the error names the file
///   3. Run the location step with `--yes` and check that it fails
///   4. Check that the sandbox and the file stay and the registry is empty
#[test]
fn move_with_extra_files_in_airlock_dir_is_refused_and_changes_nothing() {
    let home = Home::new("");
    legacy_sandbox(&home.project);
    let extra = home.project.join(".airlock/.init-done");
    std::fs::write(&extra, "").unwrap();

    let Err(err) = block_on_local(import_legacy(
        &home.context,
        &crate::test_cfg::oci::verified_dir(&home.project),
    )) else {
        panic!("import must fail");
    };
    let err = err.to_string();
    assert!(
        err.starts_with(".airlock directory contains extra files (.init-done), can't migrate"),
        "{err}"
    );
    assert!(
        block_on_local(lock_sandbox(
            &home.context,
            &crate::test_cfg::oci::verified_dir(&home.project),
            false,
            true
        ))
        .is_err()
    );

    assert!(home.project_sandbox().join("disk.img").is_file());
    assert!(extra.is_file());
    assert!(
        block_on_local(registry::list(&home.context.db))
            .unwrap()
            .is_empty()
    );
}

/// Test that a new sandbox in the data directory takes the local project
/// config from `.airlock`, so that the config is not in the repository.
///   1. Write a local project config in `.airlock` of a project without a
///      sandbox
///   2. Run the location step
///   3. Check that the config is in the new sandbox directory and that
///      `.airlock` is gone
#[test]
fn new_data_dir_sandbox_takes_local_config_out_of_project() {
    let home = Home::new("");
    std::fs::create_dir_all(home.project.join(".airlock")).unwrap();
    std::fs::write(home.project.join(".airlock/airlock.json"), "{}").unwrap();

    let dir = home.lock(false);

    assert!(dir.starts_with(home.context.data.sandboxes.to_path().unwrap()));
    assert_eq!(read(&dir, "airlock.json"), "{}");
    assert!(!home.project.join(".airlock").exists());
}

/// Test that without a terminal and without `--yes`, an older project
/// sandbox stops the start and changes nothing. A marker of the removed
/// `keep-in-project` choice does not change this. A script must not move
/// sandbox data without consent.
///   1. Make a sandbox with record files and the keep marker in the project
///   2. Run the location step without a terminal and without `--yes`
///   3. Check exit code 1, that the files in the project stay the same,
///      and that the registry and the data directory have no sandbox
///   4. Run the step with `--yes` and check that the sandbox moves
#[test]
fn old_sandbox_without_terminal_stops_start_and_changes_nothing() {
    let home = Home::new("");
    legacy_sandbox(&home.project);
    std::fs::write(home.project_sandbox().join("keep-in-project"), "").unwrap();
    let before = tree(&home.project);

    assert!(matches!(home.try_lock(false), Err(Exit::Code(1))));
    assert_eq!(tree(&home.project), before);
    assert!(home.registered().is_empty());
    let boxes = home.context.data.sandboxes.to_path().unwrap();
    assert!(std::fs::read_dir(boxes).map_or(true, |mut d| d.next().is_none()));

    let moved = home.lock(true);
    assert!(moved.starts_with(boxes));
    assert_eq!(read(&moved, "ca.json"), legacy_ca());
}

/// The files under `dir`, with their content, in name order. A symlink
/// is not followed: its content is the link target.
fn tree(dir: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    use std::os::unix::ffi::OsStrExt;
    let mut files = Vec::new();
    let mut todo = vec![dir.to_path_buf()];
    while let Some(next) = todo.pop() {
        for entry in std::fs::read_dir(&next).unwrap() {
            let entry = entry.unwrap();
            let path = entry.path();
            let kind = entry.file_type().unwrap();
            if kind.is_dir() {
                todo.push(path);
            } else if kind.is_symlink() {
                let target = std::fs::read_link(&path).unwrap();
                files.push((path, target.as_os_str().as_bytes().to_vec()));
            } else {
                files.push((path.clone(), std::fs::read(&path).unwrap()));
            }
        }
    }
    files.sort();
    files
}

/// Test that a `--yes` start imports an older project sandbox and then
/// stops at the config check. Nobody accepted the config that made the
/// imported disk. `--allow-configuration-changes` lets the start go on.
///   1. Make an older project sandbox with a disk and a known image
///   2. Start with `--yes` and check exit code 2
///   3. Check that the sandbox moved into the data directory with its disk
///   4. Start with `--yes` and `--allow-configuration-changes` and check
///      that it starts
#[test]
fn yes_start_imports_old_sandbox_and_stops_at_config_check_until_allowed() {
    let project = StartProject::new(test_packs());
    let old = legacy_sandbox(&project.project_dir());
    let (_, image_json) = cached_image(project.context(), "alpine");
    std::fs::write(old.join("image"), image_json).unwrap();

    assert!(matches!(project.start("", true), Err(Exit::Code(2))));

    let dir = project.sandbox_dir();
    assert!(dir.starts_with(project.context().data.sandboxes.to_path().unwrap()));
    assert_eq!(read(&dir, "disk.img"), "disk");
    assert!(!project.project_dir().join(".airlock").exists());

    project.start_with("", true, true).unwrap();
}

/// Test that a running project sandbox is not moved. The start fails on the
/// lock, and the sandbox stays in the project.
///   1. Make a project sandbox and hold its lock, as a running VM does
///   2. Run the step with `--yes` and check that it fails
///   3. Check that the sandbox stays and the registry is empty
#[test]
fn running_project_sandbox_is_not_moved() {
    let home = Home::new("");
    legacy_sandbox(&home.project);
    let project = VerifiedPath::new_follow(&home.project, None).unwrap();
    let old = VerifiedPath::new_follow(home.project_sandbox(), None).unwrap();
    let _running = SandboxLock::acquire_dir(&old, &project).unwrap();

    assert!(
        block_on_local(lock_sandbox(
            &home.context,
            &crate::test_cfg::oci::verified_dir(&home.project),
            false,
            true
        ))
        .is_err()
    );
    assert!(home.project_sandbox().join("disk.img").is_file());
    assert!(
        block_on_local(registry::list(&home.context.db))
            .unwrap()
            .is_empty()
    );
}

/// Test that `--yes` with `--allow-configuration-changes` does not make an
/// imported sandbox again when its old image is not known. Each image is a
/// change for it, and a re-create would delete the imported disk without a
/// question.
///   1. Make an older project sandbox with a disk and no image file
///   2. Start with `--yes` and `--allow-configuration-changes`
///   3. Check that the start stops with exit code 2, and that the imported
///      disk stays
#[test]
fn yes_start_keeps_imported_disk_when_old_image_is_unknown() {
    let project = StartProject::new(test_packs());
    legacy_sandbox(&project.project_dir());

    assert!(matches!(
        project.start_with("", true, true),
        Err(Exit::Code(2))
    ));

    let dir = project.sandbox_dir();
    assert!(dir.starts_with(project.context().data.sandboxes.to_path().unwrap()));
    assert_eq!(read(&dir, "disk.img"), "disk");
    assert_eq!(read(&dir, "image.imported"), "");
}
