//! Tests of the sandbox and install steps of `airlock start`: tool and image
//! changes, failed installs and early errors.

use crate::packs::install::state::{PackStatus, STATE_FILE};
use crate::start::Exit;
use crate::test_cfg::packs::test_packs;
use crate::test_cfg::start::{StartProject, fake_host, fake_image};

/// The config of the first start: two packs with a setup script and
/// `plain`, which has none.
const TWO: &str =
    "[packs]\nalpha = { version = 1 }\nbeta = { version = 1 }\nplain = { version = 1 }\n";
/// [`TWO`] with a pack added.
const ADDED: &str =
    "[packs]\nalpha = { version = 1 }\nbeta = { version = 1 }\ngamma = { version = 1 }\n";
/// [`TWO`] with packs removed.
const REMOVED: &str = "[packs]\nalpha = { version = 1 }\n";
/// [`TWO`] with a pack disabled.
const DISABLED: &str =
    "[packs]\nalpha = { version = 1 }\nbeta = { version = 1, enabled = false }\n";
/// [`TWO`] with other args for a pack.
const NEW_ARGS: &str =
    "[packs]\nalpha = { version = 1, args = { mode = \"slow\" } }\nbeta = { version = 1 }\n";
/// [`TWO`] with another version of a pack.
const NEW_VERSION: &str = "[packs]\nalpha = { version = 2 }\nbeta = { version = 1 }\n";

/// Take the packs whose install script ran since the last call.
fn ran() -> Vec<String> {
    fake_host(|host| std::mem::take(&mut host.ran))
}

/// Take the number of image preparations since the last call.
fn prepared() -> usize {
    fake_host(|host| std::mem::take(&mut host.prepared))
}

/// The exit code of a start. Panics on an unexpected error.
fn code(result: Result<(), Exit>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(Exit::Code(code)) => code,
        Err(Exit::Failed(e)) => panic!("{e:#}"),
    }
}

/// A project after a first start with `toml`, with the fake host
/// counters cleared.
fn started(toml: &str) -> StartProject {
    let project = StartProject::new(test_packs());
    project.start(toml, false).unwrap();
    ran();
    prepared();
    project
}

/// Test that the first start installs the packs on a new disk, and that
/// the next start with the same config installs nothing.
///   1. Start with two packs and check that both install scripts ran
///   2. Check that the state records the packs, the disk and the image
///   3. Start again and check that nothing ran and the disk and state stay
///      the same
#[test]
fn first_start_installs_packs_on_new_disk_and_next_start_installs_nothing() {
    let project = StartProject::new(test_packs());
    project.start(TWO, false).unwrap();
    assert_eq!(ran(), ["alpha", "beta"]);
    let state = project.state();
    assert!(state.is_installed("alpha") && state.is_installed("beta"));
    // `plain` has no setup script, so it has no install record.
    assert!(!state.packs.contains_key("plain"));
    assert!(state.ran_session);
    assert!(state.disk.is_some());
    assert_eq!(state.disk, project.disk_id());
    assert_eq!(state.image_id, Some(fake_image("alpine:latest").image_id));

    let disk = project.disk_id();
    project.start(TWO, false).unwrap();
    assert!(ran().is_empty());
    assert_eq!(project.disk_id(), disk);
    assert_eq!(project.state(), state);
}

/// Test that tool changes without a terminal fail with exit code 2 before
/// the image pull. Nobody can answer the question, so nothing must change.
///   1. For each kind of change, start a project, then start it again with
///      the changed config and no terminal
///   2. Check the exit code 2
///   3. Check that no image was prepared, nothing ran and the state stays
///      the same
#[test]
fn tool_changes_without_terminal_fail_before_image_pull() {
    for changed in [ADDED, REMOVED, DISABLED, NEW_ARGS, NEW_VERSION] {
        let project = started(TWO);
        let before = project.state();
        assert_eq!(code(project.start(changed, false)), 2, "{changed}");
        assert_eq!(prepared(), 0, "{changed}");
        assert!(ran().is_empty(), "{changed}");
        assert_eq!(project.state(), before, "{changed}");
    }
}

/// Test that tool changes with `--yes` re-create the sandbox and install
/// only the configured packs.
///   1. For each kind of change, start a project, then start it again with
///      the changed config and `--yes`
///   2. Check that the disk is new and that only the configured packs ran
///   3. Check that the state has only those packs, all installed, on the
///      new disk
#[test]
fn tool_changes_with_yes_re_create_sandbox_with_configured_packs() {
    for (changed, installed) in [
        (ADDED, &["alpha", "beta", "gamma"][..]),
        (REMOVED, &["alpha"][..]),
        (DISABLED, &["alpha"][..]),
        (NEW_ARGS, &["alpha", "beta"][..]),
        (NEW_VERSION, &["alpha", "beta"][..]),
    ] {
        let project = started(TWO);
        let disk = project.disk_id();
        project.start(changed, true).unwrap();
        assert_ne!(project.disk_id(), disk, "{changed}");
        assert_eq!(ran(), installed, "{changed}");
        let state = project.state();
        assert_eq!(
            state.packs.keys().collect::<Vec<_>>(),
            installed,
            "{changed}"
        );
        assert!(installed.iter().all(|id| state.is_installed(id)));
        assert_eq!(state.disk, project.disk_id());
    }
}

/// Test that an install that did not finish ends the start, and that the
/// next start tries again without a question.
///   1. Make the second pack fail, or make the install VM not confirm its
///      disk sync
///   2. Start and check exit code 1 and the failed or unconfirmed record
///   3. Start again with a working host and check that the unfinished
///      packs ran again on the same disk without a question
#[test]
fn unfinished_install_ends_start_and_next_start_retries_without_question() {
    for unsynced in [false, true] {
        let project = StartProject::new(test_packs());
        fake_host(|host| {
            host.unsynced = unsynced;
            if !unsynced {
                host.failing.insert("beta".into(), 12);
            }
        });
        assert_eq!(code(project.start(TWO, false)), 1);
        assert_eq!(ran(), ["alpha", "beta"]);
        let state = project.state();
        // No session ran after the failed install, so the next start can
        // retry without a question.
        assert!(!state.ran_session);
        let expected = if unsynced {
            PackStatus::Unconfirmed
        } else {
            PackStatus::Failed
        };
        assert_eq!(state.packs["beta"].status, expected);

        fake_host(|host| {
            host.unsynced = false;
            host.failing.clear();
        });
        let disk = project.disk_id();
        project.start(TWO, false).unwrap();
        // Without a sync, the packs that succeeded are also unconfirmed.
        let retried: &[&str] = if unsynced {
            &["alpha", "beta"]
        } else {
            &["beta"]
        };
        assert_eq!(ran(), retried);
        let state = project.state();
        assert!(state.is_installed("alpha") && state.is_installed("beta"));
        assert!(state.ran_session);
        assert_eq!(project.disk_id(), disk);
    }
}

/// Test that the next start forgets a failed pack that is no longer in the
/// config, without a question.
///   1. Start with the second pack failing
///   2. Start again without the failed pack
///   3. Check that nothing ran and that the failed record is gone
#[test]
fn failed_pack_removed_from_config_is_forgotten_without_question() {
    let project = StartProject::new(test_packs());
    fake_host(|host| host.failing.insert("beta".into(), 11));
    assert_eq!(code(project.start(TWO, false)), 1);
    fake_host(|host| host.failing.clear());
    ran();
    project.start(REMOVED, false).unwrap();
    assert!(ran().is_empty());
    let state = project.state();
    assert!(state.is_installed("alpha"));
    assert!(!state.packs.contains_key("beta"));
}

/// Test that a changed image without a terminal fails with exit code 2,
/// and that `--yes` re-creates the sandbox.
///   1. Start a project, then start it with another image and no terminal
///   2. Check exit code 2 and that nothing ran or changed
///   3. Start with another image and `--yes`
///   4. Check that the disk is new and the packs installed for the new
///      image
#[test]
fn changed_image_without_terminal_is_exit_2_and_yes_re_creates_sandbox() {
    let project = started(TWO);
    let disk = project.disk_id();
    let before = project.state();
    let other_image = format!("{TWO}[vm]\nimage = \"other:1\"\n");
    assert_eq!(code(project.start(&other_image, false)), 2);
    assert!(ran().is_empty());
    assert_eq!(project.state(), before);
    assert_eq!(project.disk_id(), disk);

    project.start(&other_image, true).unwrap();
    assert_ne!(project.disk_id(), disk);
    assert_eq!(ran(), ["alpha", "beta"]);
    let state = project.state();
    assert_eq!(state.image_id, Some(fake_image("other:1").image_id));
    assert!(state.is_installed("alpha") && state.is_installed("beta"));
}

/// Test that a corrupt or too new install state without a terminal fails
/// with exit code 2 before the image pull.
///   1. Start a project and write a bad state file
///   2. Start again and check exit code 2
///   3. Check that no image was prepared and nothing ran
#[test]
fn unreadable_install_state_without_terminal_is_exit_2() {
    for json in ["{", r#"{"version": 99}"#] {
        let project = started(TWO);
        std::fs::write(project.sandbox_dir().join(STATE_FILE), json).unwrap();
        assert_eq!(code(project.start(TWO, false)), 2, "{json}");
        assert_eq!(prepared(), 0, "{json}");
        assert!(ran().is_empty(), "{json}");
    }
}

/// Test that a missing env variable fails the start before the sandbox is
/// made.
///   1. Start with an `[env]` entry that reads an unset host variable
///   2. Check exit code 2
///   3. Check that no image was prepared and there is no sandbox
#[test]
fn missing_env_variable_fails_start_before_sandbox_is_touched() {
    let project = StartProject::new(test_packs());
    let toml = "[packs]\nalpha = { version = 1 }\n[env]\nMINE = \"${AIRLOCK_TEST_UNSET_VAR}\"\n";
    assert_eq!(code(project.start(toml, false)), 2);
    assert_eq!(prepared(), 0);
    assert!(project.sandbox().is_none());
}
