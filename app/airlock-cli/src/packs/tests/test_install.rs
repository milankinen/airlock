//! Tests of the install loop: the setup scripts run on the host in place
//! of the install VM, and the install state on disk follows each outcome.

use std::fs::File;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use indicatif::ProgressBar;

use crate::packs::InstallerScript;
use crate::packs::install::plan::{self, DecideInput, Wanted, Why};
use crate::packs::install::progress::{INSTALLS_LOG, InstallProgress};
use crate::packs::install::setup::{Ended, LoopOutcome, finish, install_loop};
use crate::packs::install::state::{self, InstallState, PackStatus, ReadState, STATE_FILE};
use crate::test_cfg::packs::{HostExec, test_installers};
use crate::test_cfg::{TempDir, block_on_local, temp_dir};
use crate::util::PinnedDir;

/// Disk identity and image of the fake install boot. Each save writes
/// them, so that the plan does not ask for a new disk.
const DISK: Option<(u64, u64)> = Some((1, 2));
const IMAGE: &str = "sha256:1";

/// Four packs out of name order. `plain` has no setup script, so the
/// installs are `alpha`, `beta` and `gamma` in name order.
const CONFIG: &str = "[packs]\ngamma = { version = 1 }\nplain = { version = 1 }\n\
                      alpha = { version = 1, args = { mode = \"slow\", fast-path = true } }\n\
                      beta = { version = 1 }\n";

/// A sandbox directory and the installs of one config.
struct Sandbox {
    tmp: TempDir,
    dir: PinnedDir,
    installers: Vec<InstallerScript>,
}

impl Sandbox {
    /// A new empty sandbox directory with the installs of `config`.
    fn new(config: &str) -> Self {
        let tmp = temp_dir();
        let dir = PinnedDir::open(tmp.path(), Path::new("sandbox"), true).unwrap();
        Self {
            tmp,
            dir,
            installers: test_installers(config),
        }
    }

    /// The path of the sandbox directory.
    fn path(&self) -> PathBuf {
        self.tmp.path().join("sandbox")
    }

    /// Run the install loop with `exec`, then update the state as after a
    /// VM shutdown with (`synced`) or without a confirmed disk sync.
    fn install(&self, exec: HostExec, synced: bool) -> (LoopOutcome, HostExec) {
        let mut exec = exec.recording_state_of(&self.path());
        let mut state = self.state();
        let dir = &self.dir;
        let mut save = |s: &mut InstallState| {
            s.disk = DISK;
            s.image_id = Some(IMAGE.into());
            state::write(dir, s)
        };
        let log = File::create(self.path().join(INSTALLS_LOG)).unwrap();
        let mut progress = InstallProgress::new(ProgressBar::hidden(), Some(log), false);
        let outcome = block_on_local(install_loop(
            &mut exec,
            &self.installers,
            &mut state,
            &mut save,
            &mut progress,
        ))
        .unwrap();
        progress.finish();
        finish(&mut state, synced, &outcome.succeeded, &mut save).unwrap();
        (outcome, exec)
    }

    /// The install state on disk (default when there is no state file).
    fn state(&self) -> InstallState {
        match state::read(&self.dir) {
            ReadState::Ok(s) => s,
            ReadState::Absent => InstallState::default(),
            other => panic!("{other:?}"),
        }
    }

    /// The packs that the next start installs according to `state`, with
    /// the reason.
    fn pending(&self, state: &InstallState) -> Vec<(String, Why)> {
        let wanted: Vec<Wanted> = self
            .installers
            .iter()
            .map(|t| Wanted {
                id: t.pack.clone(),
                fingerprint: t.fingerprint.clone(),
            })
            .collect();
        plan::decide(&DecideInput {
            state,
            wanted: &wanted,
            disk: DISK,
            image_id: Some(IMAGE),
        })
        .pending
        .into_iter()
        .map(|p| (p.id, p.why))
        .collect()
    }
}

/// The status of pack `id` in `state`.
fn status(state: &InstallState, id: &str) -> Option<PackStatus> {
    state.packs.get(id).map(|r| r.status)
}

/// Pending packs from string ids.
fn pending(items: &[(&str, Why)]) -> Vec<(String, Why)> {
    items
        .iter()
        .map(|(id, why)| ((*id).to_string(), *why))
        .collect()
}

/// Test that the install loop runs each setup script and that a synced
/// shutdown marks the packs as installed.
///   1. Install the configured packs with a synced shutdown
///   2. Check the run order and the progress messages of the status
///      channel
///   3. Check that the state file marks all packs as installed, has mode
///      0600 and leaves nothing pending
///   4. Check that the install log has the script output and status lines
///      with a pack prefix, but not the step count line
#[test]
fn installing_configured_packs_runs_setup_scripts_and_synced_shutdown_installs_them() {
    let sandbox = Sandbox::new(CONFIG);
    let (outcome, exec) = sandbox.install(HostExec::new(), true);
    assert_eq!(exec.ran, ["alpha", "beta", "gamma"]);
    assert_eq!(outcome.succeeded, ["alpha", "beta", "gamma"]);
    assert!(outcome.failed.is_empty() && outcome.stopped.is_none());
    assert_eq!(
        exec.messages[..3],
        [
            "Alpha",
            "Alpha: preparing alpha [1/2]",
            "Alpha: installing [2/2]"
        ]
    );

    let state = sandbox.state();
    for id in ["alpha", "beta", "gamma"] {
        assert!(state.is_installed(id), "{id}");
    }
    assert_eq!(state.disk, DISK);
    assert!(sandbox.pending(&state).is_empty());

    let path = sandbox.path().join(STATE_FILE);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode();
    assert_eq!(mode & 0o777, 0o600);
    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(json["packs"]["alpha"]["status"], "installed");
    assert_eq!(
        json["packs"]["alpha"]["fingerprint"],
        sandbox.installers[0].fingerprint
    );
    assert_eq!(json["disk"], serde_json::json!([1, 2]));

    let log = std::fs::read_to_string(sandbox.path().join(INSTALLS_LOG)).unwrap();
    for line in [
        "[alpha] mode=slow fast-path=true argv0=airlock-pack-alpha\n",
        "[alpha] airlock-pack: preparing alpha\n",
        "[beta] mode=fast fast-path=false argv0=airlock-pack-beta\n",
        "[gamma] airlock-pack: installing\n",
    ] {
        assert!(log.contains(line), "{line}: {log}");
    }
    // `airlock_steps` writes only to the status channel, not to the log.
    assert!(!log.contains("steps 2"), "{log}");
}

/// Test that a failed setup script does not stop the next packs, and that
/// the next start tries the failed pack again.
///   1. Configure the second pack to exit with code 11
///   2. Install and check that all three packs ran
///   3. Check that the failed pack is marked failed and is pending as a
///      retry, and that the others are installed
#[test]
fn failing_pack_does_not_stop_next_and_retries_on_next_start() {
    let sandbox = Sandbox::new(&CONFIG.replace(
        "beta = { version = 1 }",
        "beta = { version = 1, args = { mode = \"exit-11\" } }",
    ));
    let (outcome, exec) = sandbox.install(HostExec::new(), true);
    assert_eq!(exec.ran, ["alpha", "beta", "gamma"]);
    assert_eq!(outcome.failed, [("beta".to_string(), 11)]);
    let state = sandbox.state();
    assert_eq!(status(&state, "beta"), Some(PackStatus::Failed));
    assert!(state.is_installed("alpha") && state.is_installed("gamma"));
    assert_eq!(sandbox.pending(&state), pending(&[("beta", Why::Retry)]));
}

/// Test that without a confirmed disk sync, the packs stay unconfirmed and
/// the next start installs them again. The disk can lose data that the
/// guest did not sync.
///   1. Install with an unsynced shutdown
///   2. Check the state on disk when the third pack started, as after a
///      crash at that point: the first two are unconfirmed and pending as
///      retries, the third is new
///   3. Check that after the shutdown all packs are unconfirmed
#[test]
fn unsynced_shutdown_leaves_unconfirmed_records_that_retry() {
    let sandbox = Sandbox::new(CONFIG);
    let (_, exec) = sandbox.install(HostExec::new(), false);
    // The state on disk when the third exec started.
    let crashed = &exec.on_disk[2];
    assert_eq!(status(crashed, "alpha"), Some(PackStatus::Unconfirmed));
    assert_eq!(status(crashed, "beta"), Some(PackStatus::Unconfirmed));
    assert_eq!(status(crashed, "gamma"), None);
    assert_eq!(
        sandbox.pending(crashed),
        pending(&[
            ("alpha", Why::Retry),
            ("beta", Why::Retry),
            ("gamma", Why::New)
        ])
    );
    let state = sandbox.state();
    for id in ["alpha", "beta", "gamma"] {
        assert_eq!(status(&state, id), Some(PackStatus::Unconfirmed), "{id}");
    }
}

/// Test that an interrupt stops the loop, and that a synced shutdown
/// marks as installed only the packs that succeeded in this boot.
///   1. Interrupt the second pack and check that the loop stops there
///   2. Check that the first pack is installed, the second failed and
///      the third pending as new
///   3. Leave all packs unconfirmed with an unsynced install, then
///      interrupt the first pack in a synced install
///   4. Check that the first pack failed and the other two stay
///      unconfirmed, because they did not run in this boot
#[test]
fn interrupt_stops_loop_and_synced_shutdown_promotes_only_packs_that_ran() {
    let sandbox = Sandbox::new(CONFIG);
    let (outcome, exec) = sandbox.install(
        HostExec::new().stopping("beta", Some(Ended::Interrupted)),
        true,
    );
    assert_eq!(exec.ran, ["alpha", "beta"]);
    assert!(matches!(outcome.stopped, Some(Ended::Interrupted)));
    let state = sandbox.state();
    assert!(state.is_installed("alpha"));
    assert_eq!(status(&state, "beta"), Some(PackStatus::Failed));
    assert_eq!(
        sandbox.pending(&state),
        pending(&[("beta", Why::Retry), ("gamma", Why::New)])
    );

    let sandbox = Sandbox::new(CONFIG);
    sandbox.install(HostExec::new(), false);
    let (_, exec) = sandbox.install(
        HostExec::new().stopping("alpha", Some(Ended::Interrupted)),
        true,
    );
    assert_eq!(exec.ran, ["alpha"]);
    let state = sandbox.state();
    assert_eq!(status(&state, "alpha"), Some(PackStatus::Failed));
    assert_eq!(status(&state, "beta"), Some(PackStatus::Unconfirmed));
    assert_eq!(status(&state, "gamma"), Some(PackStatus::Unconfirmed));
}

/// Test that a pack whose exec did not start keeps its record. Nothing
/// ran, so nothing changed for that pack.
///   1. Make the exec of the second pack not start
///   2. Check that the loop stops as interrupted after the first pack
///   3. Check that the first pack is installed and the other two have no
///      record
#[test]
fn pack_whose_exec_did_not_start_keeps_its_record() {
    let sandbox = Sandbox::new(CONFIG);
    let (outcome, exec) = sandbox.install(HostExec::new().stopping("beta", None), true);
    assert_eq!(exec.ran, ["alpha"]);
    assert!(matches!(outcome.stopped, Some(Ended::Interrupted)));
    let state = sandbox.state();
    assert!(state.is_installed("alpha"));
    assert_eq!(status(&state, "beta"), None);
    assert_eq!(status(&state, "gamma"), None);
}

/// Test that a timeout or a failed exec stops the loop with a reason, and
/// marks the pack as failed.
///   1. For each of the two endings, end the first pack with it
///   2. Check that no other pack ran and that the stop has a reason
///   3. Check that the pack is marked failed
#[test]
fn timeout_or_failed_spawn_stops_loop_with_reason() {
    for ended in [Ended::TimedOut, Ended::ExecFailed(anyhow::anyhow!("no"))] {
        let sandbox = Sandbox::new(CONFIG);
        let (outcome, exec) = sandbox.install(HostExec::new().stopping("alpha", Some(ended)), true);
        assert_eq!(exec.ran, ["alpha"]);
        assert!(outcome.stopped.unwrap().reason().is_some());
        assert_eq!(status(&sandbox.state(), "alpha"), Some(PackStatus::Failed));
    }
}
