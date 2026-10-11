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

/// Test that a sandbox without a config copy starts without a question and
/// gets a copy. Sandboxes of an older airlock have no copy.
///   1. Start a sandbox, then remove its config copy
///   2. Start with a changed config without a terminal and check that it
///      starts
///   3. Check that the copy is of the changed config: the first config now
///      stops a start without a terminal
#[test]
fn sandbox_without_config_copy_starts_and_gets_copy() {
    let project = StartProject::new(test_packs());
    project.start(FIRST, false).unwrap();
    let run_json = project.sandbox_dir().join("run.json");
    std::fs::remove_file(&run_json).unwrap();

    project.start(CHANGED, false).unwrap();
    assert!(
        read_run_meta(&project.sandbox_dir())
            .last_run_config
            .is_some()
    );
    assert!(matches!(project.start(FIRST, false), Err(Exit::Code(2))));
}
