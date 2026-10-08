use crate::packs::install::state::{PackStatus, STATE_FILE};
use crate::start::Exit;
use crate::test_cfg::packs::test_packs;
use crate::test_cfg::start::{StartProject, fake_host, fake_image};

const TWO: &str =
    "[packs]\nalpha = { version = 1 }\nbeta = { version = 1 }\nplain = { version = 1 }\n";
const ADDED: &str =
    "[packs]\nalpha = { version = 1 }\nbeta = { version = 1 }\ngamma = { version = 1 }\n";
const REMOVED: &str = "[packs]\nalpha = { version = 1 }\n";
const DISABLED: &str =
    "[packs]\nalpha = { version = 1 }\nbeta = { version = 1, enabled = false }\n";
const NEW_ARGS: &str =
    "[packs]\nalpha = { version = 1, args = { mode = \"slow\" } }\nbeta = { version = 1 }\n";
const NEW_VERSION: &str = "[packs]\nalpha = { version = 2 }\nbeta = { version = 1 }\n";

fn ran() -> Vec<String> {
    fake_host(|host| std::mem::take(&mut host.ran))
}

fn prepared() -> usize {
    fake_host(|host| std::mem::take(&mut host.prepared))
}

fn code(result: Result<(), Exit>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(Exit::Code(code)) => code,
        Err(Exit::Failed(e)) => panic!("{e:#}"),
    }
}

fn started(toml: &str) -> StartProject {
    let project = StartProject::new(test_packs());
    project.start(toml, false).unwrap();
    ran();
    prepared();
    project
}

#[test]
fn first_start_installs_packs_on_new_disk_and_next_start_installs_nothing() {
    let project = StartProject::new(test_packs());
    project.start(TWO, false).unwrap();
    assert_eq!(ran(), ["alpha", "beta"]);
    let state = project.state();
    assert!(state.is_installed("alpha") && state.is_installed("beta"));
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

#[test]
fn missing_env_variable_fails_start_before_sandbox_is_touched() {
    let project = StartProject::new(test_packs());
    let toml = "[packs]\nalpha = { version = 1 }\n[env]\nMINE = \"${AIRLOCK_TEST_UNSET_VAR}\"\n";
    assert_eq!(code(project.start(toml, false)), 2);
    assert_eq!(prepared(), 0);
    assert!(!project.sandbox_dir().exists());
}
