//! `airlock start` with its edges faked: the image registry
//! ([`prepare_image`]), the image check ([`check_image`]) and the install
//! VM ([`install_boot`], which runs the install scripts on the host with
//! [`HostExec`]). The start steps use these in tests. What the fakes do
//! and saw is per thread ([`fake_host`]).

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

use airlock_test_utils::{TempDir, block_on_local, temp_dir};
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};

use super::context::test_context;
use super::packs::{HostExec, resolve_with};
use crate::cli::LogLevel;
use crate::config::ResolvedConfig;
use crate::config::config_values::ImageRef;
use crate::oci::{ImageChange, ImageChangeStop, OciImage, OnImageChange, PreparedImage};
use crate::packs::install::progress::InstallProgress;
use crate::packs::install::setup::{self, Ended, Report, Save, SetupError};
use crate::packs::install::state::{self, InstallState, ReadState};
use crate::packs::{InstallerScript, PackManager};
use crate::project::{self, Project};
use crate::start::{Exit, SandboxOptions};
use crate::util::PinnedDir;
use crate::vault::{Vault, VaultStorageType};

/// What the faked edges of `airlock start` do, and what they saw.
#[derive(Default)]
pub struct FakeHost {
    /// Calls of [`prepare_image`].
    pub prepared: usize,
    /// Packs whose install script ran, in order, over all install boots.
    pub ran: Vec<String>,
    /// Packs whose install exits with the code without running.
    pub failing: HashMap<String, i32>,
    /// The install VM does not confirm its disk sync.
    pub unsynced: bool,
    /// The fakes are on (else the real edges run).
    pub enabled: bool,
}

thread_local! {
    static FAKE_HOST: RefCell<FakeHost> = RefCell::default();
}

/// Read or change the [`FakeHost`] of this thread.
pub fn fake_host<R>(f: impl FnOnce(&mut FakeHost) -> R) -> R {
    FAKE_HOST.with_borrow_mut(f)
}

/// The image of the reference `name`: root, no layers, an id derived from
/// the name.
pub fn fake_image(name: &str) -> OciImage {
    OciImage {
        image_id: format!("sha256:{}", hex::encode(Sha256::digest(name))),
        name: name.to_string(),
        image_layers: vec![],
        container_home: "/root".into(),
        uid: 0,
        gid: 0,
        cmd: vec![],
        env: vec![],
        user: Some(String::new()),
    }
}

/// `oci::prepare` for tests: with the fakes on, the image is [`fake_image`] of the
/// configured name. A sandbox that had another image re-creates with
/// [`OnImageChange::Recreate`] and stops with [`OnImageChange::Refuse`].
pub async fn prepare_image(
    sandbox_dir: &Path,
    image_cfg: &ImageRef,
    vault: &Vault,
    on_change: OnImageChange,
) -> anyhow::Result<PreparedImage> {
    if !fake_host(|host| host.enabled) {
        return crate::oci::prepare(sandbox_dir, image_cfg, vault, on_change).await;
    }
    fake_host(|host| host.prepared += 1);
    let link = sandbox_dir.join("test-image");
    let name = image_cfg.name.as_str();
    let change = match std::fs::read_to_string(&link) {
        Ok(old) if old != name => match on_change {
            OnImageChange::Recreate => ImageChange::Recreate,
            OnImageChange::Refuse => return Err(ImageChangeStop::NeedsTerminal.into()),
            OnImageChange::Ask => panic!("tests have no terminal"),
        },
        _ => ImageChange::Unchanged,
    };
    std::fs::write(&link, name)?;
    Ok(PreparedImage {
        image: fake_image(name),
        change,
    })
}

/// `facts::check` for tests: with the fakes on, every image can take packs.
pub fn check_image(image: &OciImage) -> anyhow::Result<()> {
    if !fake_host(|host| host.enabled) {
        return crate::packs::install::facts::check(image);
    }
    Ok(())
}

/// `setup::install` for tests: with the fakes on, the install loop over [`HostExec`] instead
/// of an install VM, logged to the install log, and a shutdown that
/// confirms the disk sync unless [`FakeHost::unsynced`].
pub async fn install_boot(
    project: Project,
    image: &OciImage,
    installers: &[InstallerScript],
    state: &mut InstallState,
    save: &mut Save<'_>,
    log_level: LogLevel,
    verbose: bool,
) -> Result<Report, SetupError> {
    if !fake_host(|host| host.enabled) {
        return setup::install(project, image, installers, state, save, log_level, verbose).await;
    }
    let log = File::create(setup::log_path(&project.sandbox_dir)).ok();
    let mut progress = InstallProgress::new(ProgressBar::hidden(), log, verbose);
    let (failing, synced) = fake_host(|host| (host.failing.clone(), !host.unsynced));
    let mut exec = HostExec::new();
    for (id, code) in failing {
        exec = exec.stopping(&id, Some(Ended::Exited(code)));
    }
    let outcome = setup::install_loop(&mut exec, installers, state, save, &mut progress)
        .await
        .map_err(SetupError::Failed)?;
    progress.finish();
    fake_host(|host| host.ran.extend(exec.ran));
    setup::finish(state, synced, &outcome.succeeded, save).map_err(SetupError::Failed)?;
    Ok(Report { outcome, synced })
}

/// A project directory (and a home for its context) that `airlock start`
/// runs in, with the faked edges.
pub struct StartProject {
    dir: TempDir,
    home: TempDir,
    packs: PackManager,
}

impl StartProject {
    /// A project without a sandbox, configured against `packs`. Resets
    /// the [`FakeHost`] of this thread.
    pub fn new(packs: PackManager) -> Self {
        fake_host(|host| {
            *host = FakeHost {
                enabled: true,
                ..FakeHost::default()
            };
        });
        Self {
            dir: temp_dir(),
            home: temp_dir(),
            packs,
        }
    }

    pub fn sandbox_dir(&self) -> PathBuf {
        self.dir.path().join(".airlock/sandbox")
    }

    /// The sandbox and install steps of `airlock start` with the project
    /// file `toml`, without a terminal; `yes`: `--yes`. The sandbox lock
    /// of the previous start can stay held for a moment after it is
    /// dropped: a process that a parallel test spawns has a copy of its
    /// descriptor until it execs. A start that finds the lock held tries
    /// again.
    pub fn start(&self, toml: &str, yes: bool) -> Result<(), Exit> {
        let resolved = resolve_with(&self.packs, toml).map_err(Exit::config)?;
        let options = SandboxOptions {
            yes,
            log_level: LogLevel::Info,
            verbose: false,
        };
        for _ in 0..200 {
            match self.start_once(&resolved, &options) {
                Err(Exit::Failed(e)) if e.to_string().starts_with("another airlock instance") => {
                    std::thread::sleep(Duration::from_millis(5));
                }
                result => return result,
            }
        }
        panic!("the sandbox lock stays held");
    }

    fn start_once(&self, resolved: &ResolvedConfig, options: &SandboxOptions) -> Result<(), Exit> {
        let vault = Vault::for_storage_type(VaultStorageType::Disabled);
        block_on_local(async {
            let sandbox = crate::start::sandbox::ensure_sandbox(
                self.dir.path(),
                &self.packs,
                resolved,
                options,
                &vault,
            )
            .await?;
            let context = test_context(self.home.path(), vault.clone());
            let project = project::open(&sandbox.lock, resolved.values.clone(), None, context)?;
            crate::start::install::install_tools(
                &project,
                resolved,
                &sandbox.image,
                sandbox.installs,
                options,
            )
            .await
        })
    }

    /// The install state of the sandbox (empty when there is none).
    pub fn state(&self) -> InstallState {
        let dir = PinnedDir::open(self.dir.path(), Path::new(".airlock/sandbox"), true).unwrap();
        match state::read(&dir) {
            ReadState::Ok(state) => state,
            ReadState::Absent => InstallState::default(),
            other => panic!("{other:?}"),
        }
    }

    /// The identity of the sandbox disk.
    pub fn disk_id(&self) -> Option<(u64, u64)> {
        project::disk_id(&self.sandbox_dir())
    }
}
