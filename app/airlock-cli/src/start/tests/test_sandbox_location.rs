//! Tests of the sandbox location step of `airlock start`: where a new
//! sandbox goes, and the move of a sandbox from the project directory into
//! the data directory.

use std::path::{Path, PathBuf};

use crate::context::Context;
use crate::project::SandboxLock;
use crate::sandboxes::{KEEP_IN_PROJECT, Location, migrate, registry};
use crate::start::location::lock_sandbox;
use crate::test_cfg::packs::test_packs;
use crate::test_cfg::sandboxes::{LEGACY_CA, legacy_sandbox};
use crate::test_cfg::start::StartProject;
use crate::test_cfg::{TempDir, block_on_local, temp_dir, test_context};
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

    /// Run the location step without a terminal. `yes` is `--yes`.
    ///
    /// The lock of a previous step can stay held for a short time after it
    /// drops: a process that a parallel test starts has a copy of the lock
    /// descriptor until it execs. Thus a step that finds the lock held tries
    /// again (as [`StartProject::start`] does).
    /// Returns:
    ///   The sandbox directory. The lock is released.
    fn lock(&self, yes: bool) -> PathBuf {
        for _ in 0..200 {
            match block_on_local(lock_sandbox(&self.context, &self.project, false, yes)) {
                Ok(lock) => return lock.dir().to_path_buf(),
                Err(e) if format!("{e:?}").contains("another airlock instance") => {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                Err(e) => panic!("{e:?}"),
            }
        }
        panic!("the sandbox lock stays held");
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
    let Location::DataDir { id } = &found.location else {
        panic!("{found:?}");
    };
    assert_eq!(found.dir, project.context().boxes_dir().join(id));
    assert!(!project.project_dir().join(".airlock").exists());
    for name in ["ca.json", "run.json", "installs.json"] {
        assert!(found.dir.join(name).is_file(), "{name}");
    }

    project.start(ONE, false).unwrap();
    let entries = block_on_local(registry::list(&project.context().db)).unwrap();
    assert_eq!(entries.len(), 1);
    assert_eq!(&entries[0].id, id);
}

/// Test that the `project-owned` setting puts a new sandbox in the project,
/// and keeps it out of the registry.
///   1. Set `sandbox_type = "project-owned"` and start a project
///   2. Check that the sandbox is in `.airlock/sandbox` and has a CA
///   3. Check that the registry is empty
#[test]
fn project_dir_setting_puts_new_sandbox_in_project() {
    let project = StartProject::with_settings(
        test_packs(),
        "[security]\nsandbox_type = \"project-owned\"\n",
    );
    project.start(ONE, false).unwrap();

    let found = project.sandbox().unwrap();
    assert_eq!(found.location, Location::ProjectDir);
    assert_eq!(found.dir, project.project_dir().join(".airlock/sandbox"));
    assert!(found.dir.join("ca.json").is_file());
    assert!(
        block_on_local(registry::list(&project.context().db))
            .unwrap()
            .is_empty()
    );
}

/// Test that `--yes` moves an older project sandbox into the data
/// directory with its disk, records and local project config, and removes
/// the `.airlock` directory of the project.
///   1. Make a sandbox with record files and a disk in the project, and a
///      local project config, a `.gitignore` and a log next to it
///   2. Run the location step with `--yes`
///   3. Check that the sandbox is registered and is in the data directory
///   4. Check that the disk, the record files and the local config moved
///   5. Check that the `.airlock` directory of the project is gone
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
    assert_eq!(entries[0].project, home.project);
    assert_eq!(dir, home.context.boxes_dir().join(&entries[0].id));
    assert_eq!(read(&dir, "disk.img"), "disk");
    assert_eq!(read(&dir, "ca.json"), LEGACY_CA);
    assert!(dir.join("run.json").is_file() && dir.join("installs.json").is_file());
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

    let err = block_on_local(migrate(&home.context, &home.project))
        .unwrap_err()
        .to_string();
    assert!(
        err.starts_with(".airlock directory contains extra files (.init-done), can't migrate"),
        "{err}"
    );
    assert!(block_on_local(lock_sandbox(&home.context, &home.project, false, true)).is_err());

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

    assert!(dir.starts_with(home.context.boxes_dir()));
    assert_eq!(read(&dir, "airlock.json"), "{}");
    assert!(!home.project.join(".airlock").exists());
}

/// Test that without a terminal and without `--yes`, an older project
/// sandbox stays in the project, and that a later start can still move it.
///   1. Make a sandbox with record files in the project
///   2. Run the location step without a terminal and without `--yes`
///   3. Check that the sandbox stays in the project, without the keep
///      marker
///   4. Run the step with `--yes` and check that the sandbox moves
#[test]
fn project_sandbox_without_terminal_stays_and_later_start_can_move_it() {
    let home = Home::new("");
    legacy_sandbox(&home.project);

    assert_eq!(home.lock(false), home.project_sandbox());
    assert_eq!(read(&home.project_sandbox(), "ca.json"), LEGACY_CA);
    assert!(!home.project_sandbox().join(KEEP_IN_PROJECT).exists());

    let moved = home.lock(true);
    assert!(moved.starts_with(home.context.boxes_dir()));
    assert_eq!(read(&moved, "ca.json"), LEGACY_CA);
}

/// Test that a project sandbox stays in the project when the user chose to
/// keep it there, or when the settings select the project directory.
///   1. Mark a project sandbox as kept (the "no" answer) and run the step
///      with `--yes`, and check that it stays
///   2. With `sandbox_type = "project-owned"`, run the step with `--yes`
///      on an older project sandbox, and check that it stays
///   3. Check that the registry stays empty in both cases
#[test]
fn kept_project_sandbox_or_project_dir_setting_is_not_moved() {
    let kept = Home::new("");
    legacy_sandbox(&kept.project);
    std::fs::write(kept.project_sandbox().join(KEEP_IN_PROJECT), "").unwrap();
    assert_eq!(kept.lock(true), kept.project_sandbox());

    let setting = Home::new("[security]\nsandbox_type = \"project-owned\"\n");
    legacy_sandbox(&setting.project);
    assert_eq!(setting.lock(true), setting.project_sandbox());

    for home in [&kept, &setting] {
        assert!(
            block_on_local(registry::list(&home.context.db))
                .unwrap()
                .is_empty()
        );
    }
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
    let _running = SandboxLock::acquire(&home.project).unwrap();

    assert!(block_on_local(lock_sandbox(&home.context, &home.project, false, true)).is_err());
    assert!(home.project_sandbox().join("disk.img").is_file());
    assert!(
        block_on_local(registry::list(&home.context.db))
            .unwrap()
            .is_empty()
    );
}
