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

const DISK: Option<(u64, u64)> = Some((1, 2));
const IMAGE: &str = "sha256:1";

const CONFIG: &str = "[packs]\ngamma = { version = 1 }\nplain = { version = 1 }\n\
                      alpha = { version = 1, args = { mode = \"slow\", fast-path = true } }\n\
                      beta = { version = 1 }\n";

struct Sandbox {
    tmp: TempDir,
    dir: PinnedDir,
    installers: Vec<InstallerScript>,
}

impl Sandbox {
    fn new(config: &str) -> Self {
        let tmp = temp_dir();
        let dir = PinnedDir::open(tmp.path(), Path::new("sandbox"), true).unwrap();
        Self {
            tmp,
            dir,
            installers: test_installers(config),
        }
    }

    fn path(&self) -> PathBuf {
        self.tmp.path().join("sandbox")
    }

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

    fn state(&self) -> InstallState {
        match state::read(&self.dir) {
            ReadState::Ok(s) => s,
            ReadState::Absent => InstallState::default(),
            other => panic!("{other:?}"),
        }
    }

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

fn status(state: &InstallState, id: &str) -> Option<PackStatus> {
    state.packs.get(id).map(|r| r.status)
}

fn pending(items: &[(&str, Why)]) -> Vec<(String, Why)> {
    items
        .iter()
        .map(|(id, why)| ((*id).to_string(), *why))
        .collect()
}

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
    assert!(!log.contains("steps 2"), "{log}");
}

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

#[test]
fn unsynced_shutdown_leaves_unconfirmed_records_that_retry() {
    let sandbox = Sandbox::new(CONFIG);
    let (_, exec) = sandbox.install(HostExec::new(), false);
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
