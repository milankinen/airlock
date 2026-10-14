//! `airlock start` with fakes at its edges: the image registry, the image
//! check and the install VM. The fake install VM runs the install scripts
//! on the host. The start steps call these fakes in tests. Each thread has
//! its own fake state.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs::File;
use std::path::PathBuf;

use airlock_test_utils::{TempDir, block_on_local, eventually, temp_dir};
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};

use super::context::test_context;
use super::packs::HostExec;
use crate::cli::LogLevel;
use crate::config::config_values::ImageRef;
use crate::config::{ConfigOverrides, LayeredConfig};
use crate::context::Context;
use crate::data_dir::DataDir;
use crate::oci::{ImageChange, ImageChangeStop, OciImage, OnImageChange, PreparedImage};
use crate::packs::install::progress::InstallProgress;
use crate::packs::install::setup::{self, Ended, Report, Save, SetupError};
use crate::packs::install::state::{self, InstallState, ReadState};
use crate::packs::{InstallerScript, PackManager};
use crate::project::{self, Project};
use crate::sandboxes::{self, Found};
use crate::start::{Exit, SandboxOptions, config_change};
use crate::util::PinnedDir;
use crate::util::verified_path::VerifiedPath;
use crate::vault::{Vault, VaultStorageType};

/// What the fakes at the edges of `airlock start` do, and what they
/// recorded.
#[derive(Default)]
pub struct FakeHost {
    /// Calls of [`prepare_image`].
    pub prepared: usize,
    /// Packs whose install script ran, in order, over all install boots.
    pub ran: Vec<String>,
    /// Packs whose install exits with the given code and does not run.
    pub failing: HashMap<String, i32>,
    /// The install VM does not confirm its disk sync.
    pub unsynced: bool,
    /// The fakes are on. Else the real edges run.
    pub enabled: bool,
}

thread_local! {
    static FAKE_HOST: RefCell<FakeHost> = RefCell::default();
}

/// Read or change the [`FakeHost`] of this thread.
pub fn fake_host<R>(f: impl FnOnce(&mut FakeHost) -> R) -> R {
    FAKE_HOST.with_borrow_mut(f)
}

/// A fake image for the reference `name`: user root, no layers, and an ID
/// made from the name.
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

/// `oci::prepare` for tests. With the fakes on, the image is the
/// [`fake_image`] of the configured name. If the sandbox had another image,
/// [`OnImageChange::Recreate`] makes it again and [`OnImageChange::Refuse`]
/// stops.
pub async fn prepare_image(
    data_dir: &DataDir,
    sandbox_dir: &VerifiedPath,
    image_cfg: &ImageRef,
    vault: &Vault,
    on_change: OnImageChange,
) -> anyhow::Result<PreparedImage> {
    if !fake_host(|host| host.enabled) {
        return crate::oci::prepare(data_dir, sandbox_dir, image_cfg, vault, on_change).await;
    }
    fake_host(|host| host.prepared += 1);
    let link = sandbox_dir.to_path()?.join("test-image");
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
    let mut image = fake_image(name);
    // A pinned digest names the image, as with a real registry.
    if let Some(digest) = image_cfg.pinned_digest() {
        image.image_id = digest.to_string();
    }
    let pin = Some(image.image_id.clone());
    Ok(PreparedImage { image, change, pin })
}

/// `facts::check` for tests. With the fakes on, each image can take packs.
pub fn check_image(data_dir: &DataDir, image: &OciImage) -> anyhow::Result<()> {
    if !fake_host(|host| host.enabled) {
        return crate::packs::install::facts::check(data_dir, image);
    }
    Ok(())
}

/// `setup::install` for tests. With the fakes on, it runs the install loop
/// with [`HostExec`] instead of an install VM and writes the install log.
/// The fake shutdown confirms the disk sync unless [`FakeHost::unsynced`]
/// is set.
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
    let log = File::create(
        setup::log_path(&project.sandbox_dir)
            .unwrap()
            .to_path()
            .unwrap(),
    )
    .ok();
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

/// The config with one project file `airlock.toml` with the content `toml`.
fn project_config(toml: &str) -> LayeredConfig {
    LayeredConfig::from_values(
        vec![],
        None,
        vec![("airlock.toml", toml::from_str(toml).unwrap())],
    )
    .unwrap()
}

/// A project directory and a home directory for `airlock start` with the
/// fakes on. The home is also the data directory, so the sandbox goes there.
pub struct StartProject {
    dir: TempDir,
    // Keep the home directory while the context uses it.
    _home: TempDir,
    context: Context,
    packs: PackManager,
}

impl StartProject {
    /// A project with no sandbox that uses the packs `packs`. Resets the
    /// [`FakeHost`] of this thread and turns the fakes on.
    pub fn new(packs: PackManager) -> Self {
        Self::with_settings(packs, "")
    }

    /// A project as with [`Self::new`], and the user settings file
    /// `settings` (TOML).
    pub fn with_settings(packs: PackManager, settings: &str) -> Self {
        fake_host(|host| {
            *host = FakeHost {
                enabled: true,
                ..FakeHost::default()
            };
        });
        let home = temp_dir();
        std::fs::write(home.path().join("settings.toml"), settings).unwrap();
        let context = test_context(
            home.path(),
            Vault::for_storage_type(VaultStorageType::Disabled),
        );
        Self {
            dir: temp_dir(),
            _home: home,
            context,
            packs,
        }
    }

    /// The process context of the starts.
    pub fn context(&self) -> &Context {
        &self.context
    }

    /// The canonical project directory.
    pub fn project_dir(&self) -> PathBuf {
        std::fs::canonicalize(self.dir.path()).unwrap()
    }

    /// The sandbox of the project, if it has one.
    pub fn sandbox(&self) -> Option<Found> {
        block_on_local(sandboxes::resolve_sandbox(
            &self.context,
            &super::oci::verified_dir(&self.project_dir()),
            false,
            false,
        ))
        .unwrap()
    }

    /// The sandbox directory of the project. Panics if there is no sandbox.
    pub fn sandbox_dir(&self) -> PathBuf {
        let dir = self.sandbox().expect("a sandbox").dir;
        dir.to_path().unwrap().to_path_buf()
    }

    /// Run the sandbox and install steps of `airlock start` with the
    /// project file `toml` and no terminal. `yes` is the `--yes` flag.
    ///
    /// The sandbox lock of the previous start can stay held for a short
    /// time after it drops. The cause is a process that a parallel test
    /// starts: it has a copy of the lock descriptor until it execs. Thus a
    /// start waits until the lock is free, and tries again if it finds the
    /// lock held.
    pub fn start(&self, toml: &str, yes: bool) -> Result<(), Exit> {
        self.start_with(toml, yes, false)
    }

    /// [`Self::start`] with `allow_config_changes`, the
    /// `--allow-configuration-changes` flag.
    pub fn start_with(
        &self,
        toml: &str,
        yes: bool,
        allow_config_changes: bool,
    ) -> Result<(), Exit> {
        self.start_config(&project_config(toml), yes, allow_config_changes)
    }

    /// [`Self::start_with`] with the config files `config`, for example a
    /// config of the setup wizard.
    pub fn start_config(
        &self,
        config: &LayeredConfig,
        yes: bool,
        allow_config_changes: bool,
    ) -> Result<(), Exit> {
        let options = SandboxOptions {
            yes,
            log_level: LogLevel::Info,
            verbose: false,
            allow_config_changes,
            overrides: ConfigOverrides::default(),
        };
        let project_sandbox = self.project_dir().join(sandboxes::PROJECT_SANDBOX);
        eventually("sandbox lock is free", || {
            // The start does not move a running project sandbox, and does
            // not fail.
            let dir = crate::util::VerifiedPath::new_follow(&project_sandbox, None).unwrap();
            if project::is_running(&dir) {
                return None;
            }
            match self.start_once(config, &options) {
                Err(Exit::Failed(e)) if e.to_string().starts_with("another airlock instance") => {
                    None
                }
                result => Some(result),
            }
        })
    }

    /// One try of [`Self::start`].
    fn start_once(&self, config: &LayeredConfig, options: &SandboxOptions) -> Result<(), Exit> {
        block_on_local(async {
            let resolved = config
                .resolve(&self.context.data, &self.packs, &options.overrides)
                .await
                .map_err(Exit::config)?;
            let template = config
                .generated_project()
                .and_then(|g| g.template.as_deref())
                .map(|name| crate::templates::find(&self.context.data, name).unwrap());
            let sandbox = crate::start::sandbox::ensure_sandbox(
                &self.context,
                &super::oci::verified_dir(&self.project_dir()),
                &self.packs,
                config,
                resolved,
                options,
                template.as_ref(),
            )
            .await?;
            let resolved = &sandbox.resolved;
            let project = project::open(
                &sandbox.lock,
                resolved.values.clone(),
                None,
                self.context.clone(),
            )?;
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

    /// Save the config of the project file `toml` as the config of the
    /// last start. Thus the next start with `toml` gets no config question
    /// (see [`crate::start::config_change`]). Panics if there is no
    /// sandbox.
    pub fn accept_config(&self, toml: &str) {
        let dir = self.sandbox().expect("a sandbox").dir;
        let config = project_config(toml);
        let resolved = block_on_local(config.resolve(
            &self.context.data,
            &self.packs,
            &ConfigOverrides::default(),
        ))
        .unwrap();
        let cwd = super::oci::verified_dir(&self.project_dir());
        let paths = config_change::host_paths(&resolved.values, &self.context.home, &cwd);
        config_change::save(&dir, &config, &paths).unwrap();
    }

    /// The install state of the sandbox. Empty when there is no state.
    pub fn state(&self) -> InstallState {
        let dir = PinnedDir::pin(&self.sandbox_dir()).unwrap();
        match state::read(&dir) {
            ReadState::Ok(state) => state,
            ReadState::Absent => InstallState::default(),
            other => panic!("{other:?}"),
        }
    }

    /// Build the template `name` from the config files `config` (see
    /// [`crate::templates::create::build_template`]). `replace` replaces a
    /// template with the same name.
    pub fn build_template(
        &self,
        name: &str,
        config: &LayeredConfig,
        replace: bool,
    ) -> Result<(), Exit> {
        let options = SandboxOptions {
            yes: true,
            log_level: LogLevel::Info,
            verbose: false,
            allow_config_changes: false,
            overrides: ConfigOverrides::default(),
        };
        block_on_local(crate::templates::create::build_template(
            &self.context,
            name,
            &self.packs,
            config,
            &options,
            replace,
        ))
    }

    /// The identity of the sandbox disk.
    pub fn disk_id(&self) -> Option<(u64, u64)> {
        project::disk_id(&PinnedDir::pin(&self.sandbox_dir()).unwrap())
    }
}
