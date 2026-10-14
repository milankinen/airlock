//! Tests of the config change check of `airlock start`.

use crate::project::read_run_meta;
use crate::start::Exit;
use crate::test_cfg::packs::test_packs;
use crate::test_cfg::start::{StartProject, fake_host};

/// The config of the first start.
const FIRST: &str = "[vm]\ncpus = 2\n";
/// [`FIRST`] with a changed value.
const CHANGED: &str = "[vm]\ncpus = 3\n";

/// Take the number of image preparations since the last call.
fn prepared() -> usize {
    fake_host(|host| std::mem::take(&mut host.prepared))
}

/// Test that a changed config stops a start without a terminal and with
/// `--yes`, before the image step. A start from code in the sandbox or from
/// a script must not use a config that nobody accepted.
///   1. Start a new sandbox, then start again with the same config
///   2. Start with a changed config without and with `--yes`, and check
///      exit code 2 and no image step
///   3. Start with the changed config, `--yes` and
///      `--allow-configuration-changes`, and check that it starts
///   4. Start with the changed config without the flags, and check that it
///      starts because the copy is now of the changed config
#[test]
fn changed_config_stops_start_until_allowed() {
    let project = StartProject::new(test_packs());
    project.start(FIRST, false).unwrap();
    project.start(FIRST, false).unwrap();
    prepared();

    for yes in [false, true] {
        assert!(matches!(project.start(CHANGED, yes), Err(Exit::Code(2))));
    }
    assert_eq!(prepared(), 0);
    project.start_with(CHANGED, true, true).unwrap();
    project.start(CHANGED, false).unwrap();
}

/// Test that a sandbox with a disk but no config copy stops a start without
/// a terminal. Nobody accepted the config that made the disk, for example
/// in an imported sandbox of an older airlock.
///   1. Start a sandbox, check that it has a disk, then remove its config
///      copy
///   2. Start without a terminal and check exit code 2
///   3. Start with `--allow-configuration-changes` and check that it
///      starts and gets a copy
///   4. Start again without the flag and check that it starts
#[test]
fn sandbox_with_disk_without_config_copy_stops_start_until_allowed() {
    let project = StartProject::new(test_packs());
    project.start(FIRST, false).unwrap();
    assert!(project.sandbox_dir().join("disk.img").is_file());
    std::fs::remove_file(project.sandbox_dir().join("run.json")).unwrap();

    assert!(matches!(project.start(FIRST, false), Err(Exit::Code(2))));
    project.start_with(FIRST, false, true).unwrap();
    assert!(
        read_run_meta(&project.sandbox().unwrap().dir)
            .last_run_config
            .is_some()
    );
    project.start(FIRST, false).unwrap();
}

/// Test that a new sandbox starts without a terminal and without a
/// question. It has no disk yet, thus no config made its state.
///   1. Start a new project without a terminal and check that it starts
///   2. Check that the sandbox has a config copy and a disk now
#[test]
fn new_sandbox_without_disk_starts_without_question() {
    let project = StartProject::new(test_packs());
    project.start(FIRST, false).unwrap();
    assert!(
        read_run_meta(&project.sandbox().unwrap().dir)
            .last_run_config
            .is_some()
    );
    assert!(project.sandbox_dir().join("disk.img").is_file());
}

/// Test that a mount source symlink that points to an other dir since the
/// last start stops a start without a terminal. Code in the sandbox can
/// change the symlink in the project dir. A `run.json` of an older airlock
/// without host paths gives no stop.
///   1. Make the dirs `a` and `b` and the symlink `data` to `a` in the
///      project, and start two times with a mount of `data`
///   2. Change the symlink to `b` and check exit code 2
///   3. Remove the host paths from `run.json` and check that the start
///      runs
#[test]
fn retargeted_mount_source_symlink_stops_start() {
    let project = StartProject::new(test_packs());
    let dir = project.project_dir();
    std::fs::create_dir(dir.join("a")).unwrap();
    std::fs::create_dir(dir.join("b")).unwrap();
    std::os::unix::fs::symlink("a", dir.join("data")).unwrap();
    let toml = "[mounts.data]\nsource = \"./data\"\ntarget = \"/data\"\n";
    project.start(toml, false).unwrap();
    project.start(toml, false).unwrap();

    std::fs::remove_file(dir.join("data")).unwrap();
    std::os::unix::fs::symlink("b", dir.join("data")).unwrap();
    assert!(matches!(project.start(toml, false), Err(Exit::Code(2))));

    let run_json = project.sandbox_dir().join("run.json");
    let mut meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&run_json).unwrap()).unwrap();
    meta.as_object_mut()
        .unwrap()
        .remove("last_run_paths")
        .unwrap();
    std::fs::write(&run_json, meta.to_string()).unwrap();
    project.start(toml, false).unwrap();
}
