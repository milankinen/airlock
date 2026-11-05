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
use crate::config::{ConfigOverrides, LayeredConfig, ResolveInput};
use crate::context::Context;
use crate::data_dir::DataDir;
use crate::oci::{ImageChange, ImageChangeStop, OciImage, OnImageChange, PreparedImage};
use crate::packs::install::progress::InstallProgress;
use crate::packs::install::setup::{self, Ended, Report, Save, SetupError};
use crate::packs::install::state::{self, InstallState, ReadState};
use crate::packs::{InstallerScript, PackManager};
use crate::sandbox::first_start::FirstStart;
use crate::sandbox::start::ensure::ChangedAnswer;
use crate::sandbox::start::{Exit, SandboxOptions, config_change, location};
use crate::sandbox::{self, BootOptions, FindOpts, Project, Sandbox, SandboxLock};
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
    /// The answer to the changed-packs question. If set, the starts have
    /// a terminal, and [`choose_changed`] answers with it.
    pub changed_answer: Option<ChangedAnswer>,
    /// The lines of each changed-packs question that [`choose_changed`]
    /// answered, with no colors.
    pub changed_questions: Vec<Vec<String>>,
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
/// [`fake_image`] of the configured name. An import marker counts as
/// another image. If the sandbox had another image,
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
    // An imported sandbox has the old image in the import marker. A
    // marker never names a fake image, thus it is a change.
    let stored = std::fs::read_to_string(&link)
        .ok()
        .or_else(|| crate::oci::read_import_marker(sandbox_dir).map(|_| String::new()));
    let change = match stored {
        Some(old) if old != name => match crate::oci::guard_recreate(sandbox_dir, on_change)? {
            OnImageChange::Recreate => ImageChange::Recreate,
            OnImageChange::Refuse => return Err(ImageChangeStop::NeedsTerminal.into()),
            OnImageChange::Ask => panic!("tests have no terminal"),
        },
        _ => ImageChange::Unchanged,
    };
    std::fs::write(&link, name)?;
    // As `oci::prepare`, which removes the marker after the image link.
    let _ = std::fs::remove_file(sandbox_dir.to_path()?.join("image.imported"));
    let mut image = fake_image(name);
    // A pinned digest names the image, as with a real registry.
    if let Some(digest) = image_cfg.pinned_digest() {
        image.image_id = digest.to_string();
    }
    let pin = Some(image.image_id.clone());
    Ok(PreparedImage { image, change, pin })
}

/// The changed-packs question for tests. With
/// [`FakeHost::changed_answer`], it records the lines of the question and
/// returns the answer. Else the real question runs.
pub fn choose_changed(items: &[String], lines: &[String]) -> Result<ChangedAnswer, Exit> {
    let Some(answer) = fake_host(|host| host.changed_answer) else {
        return crate::sandbox::start::ensure::choose_changed_terminal(items, lines);
    };
    let plain = lines
        .iter()
        .map(|line| console::strip_ansi_codes(line).into_owned())
        .collect();
    fake_host(|host| host.changed_questions.push(plain));
    Ok(answer)
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
    boot: BootOptions,
    verbose: bool,
) -> Result<Report, SetupError> {
    if !fake_host(|host| host.enabled) {
        return setup::install(project, image, installers, state, save, boot, verbose).await;
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

/// Find or register the sandbox of the project `host_cwd` as the `start`
/// client does, and take its locks as the sandbox process holds them.
/// Returns:
///   The held sandbox locks, or the exit of a step.
pub async fn lock_sandbox(context: &Context, host_cwd: &VerifiedPath) -> Result<SandboxLock, Exit> {
    let sandbox = location::find_or_register(context, host_cwd).await?;
    Ok(SandboxLock::acquire_box(
        host_cwd,
        &context.data.sandboxes,
        sandbox.id(),
    )?)
}

/// Run the location steps of `airlock start` for the project `host_cwd`:
/// the client takes the locks, and the sandbox process fills a new sandbox.
/// A failed fill removes a new sandbox, as in the sandbox process.
/// Args:
///  - `context`: Process context
///  - `host_cwd`: Canonical project directory
///  - `can_prompt`: A terminal is available for questions
///  - `yes`: `--yes`
///
/// Returns:
///   The held sandbox locks, or the exit of a step.
pub async fn locate_sandbox(
    context: &Context,
    host_cwd: &VerifiedPath,
    can_prompt: bool,
    yes: bool,
) -> Result<SandboxLock, Exit> {
    let lock = lock_sandbox(context, host_cwd).await?;
    let mut first = FirstStart::check(&lock);
    if let Err(e) = location::fill_new_sandbox(context, &lock, &mut first, can_prompt, yes) {
        first
            .remove(&context.db, &lock, &super::sandboxes::lock_id(&lock))
            .await;
        return Err(e);
    }
    Ok(lock)
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

    /// Use the packs `packs` in the next starts, for example after a
    /// change of a user pack.
    pub fn set_packs(&mut self, packs: PackManager) {
        self.packs = packs;
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
    pub fn sandbox(&self) -> Option<Sandbox> {
        block_on_local(sandbox::find(
            &self.context,
            &super::oci::verified_dir(&self.project_dir()),
            FindOpts::default(),
        ))
        .unwrap()
    }

    /// The sandbox directory of the project. Panics if there is no sandbox.
    pub fn sandbox_dir(&self) -> PathBuf {
        let dir = self.sandbox().expect("a sandbox").dir().clone();
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
            can_prompt: crate::cli::prompt::can_prompt()
                || fake_host(|host| host.changed_answer.is_some()),
            log_level: LogLevel::Info,
            verbose: false,
            allow_config_changes,
            overrides: ConfigOverrides::default(),
            auto_approve_packs: false,
            vmm: crate::vm::load_backend(crate::settings::Vmm::HOST_DEFAULT).unwrap(),
        };
        eventually("sandbox lock is free", || {
            // Wait until the lock of a registered sandbox is free.
            if self.sandbox().is_some_and(|found| found.is_running()) {
                return None;
            }
            match self.start_once(config, &options) {
                Err(Exit::Failed(e))
                    if [
                        crate::messages::start::START_ALREADY_RUNNING,
                        crate::messages::project::SANDBOX_IN_USE,
                    ]
                    .contains(&e.to_string().as_str()) =>
                {
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
                .resolve(&ResolveInput {
                    data_dir: &self.context.data,
                    packs: &self.packs,
                    overrides: &options.overrides,
                    project_root: Some(&super::oci::verified_dir(&self.project_dir())),
                    approval: crate::packs::approval::ApprovalMode::APPROVE,
                })
                .await
                .map_err(Exit::config)?;
            let template = config
                .generated_project()
                .and_then(|g| g.template.as_deref())
                .map(|name| crate::templates::find(&self.context.data, name).unwrap());
            // As in `airlock start`: the client takes the locks, and the
            // sandbox process removes a new sandbox after a failed start.
            let host_cwd = super::oci::verified_dir(&self.project_dir());
            let lock = lock_sandbox(&self.context, &host_cwd).await?;
            let mut first = FirstStart::check(&lock);
            let result = self
                .prepare_locked(&lock, &mut first, config, resolved, options, template)
                .await;
            if result.is_err() {
                let id = super::sandboxes::lock_id(&lock);
                first.remove(&self.context.db, &lock, &id).await;
            }
            result
        })
    }

    /// The steps of [`Self::start_once`] in the sandbox process, under the
    /// held locks `lock`.
    async fn prepare_locked(
        &self,
        lock: &SandboxLock,
        first: &mut FirstStart,
        config: &LayeredConfig,
        resolved: crate::config::ResolvedConfig,
        options: &SandboxOptions,
        template: Option<crate::templates::Template>,
    ) -> Result<(), Exit> {
        location::fill_new_sandbox(&self.context, lock, first, options.can_prompt, options.yes)?;
        let sandbox = crate::sandbox::start::ensure::ensure_sandbox(
            &self.context,
            lock,
            &self.packs,
            config,
            resolved,
            options,
            template.as_ref(),
        )
        .await?;
        let resolved = &sandbox.resolved;
        let project = sandbox::open(lock, resolved.values.clone(), None, self.context.clone())?;
        crate::sandbox::start::install::install_tools(
            &project,
            resolved,
            &sandbox.image,
            sandbox.installs,
            options,
        )
        .await
    }

    /// Save the config of the project file `toml` as the config of the
    /// last start. Thus the next start with `toml` gets no config question
    /// (see [`crate::sandbox::start::config_change`]). Panics if there is no
    /// sandbox.
    pub fn accept_config(&self, toml: &str) {
        let dir = self.sandbox().expect("a sandbox").dir().clone();
        let config = project_config(toml);
        let resolved = block_on_local(config.resolve(&ResolveInput {
            data_dir: &self.context.data,
            packs: &self.packs,
            overrides: &ConfigOverrides::default(),
            project_root: Some(&super::oci::verified_dir(&self.project_dir())),
            approval: crate::packs::approval::ApprovalMode::APPROVE,
        }))
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
            can_prompt: crate::cli::prompt::can_prompt(),
            log_level: LogLevel::Info,
            verbose: false,
            allow_config_changes: false,
            overrides: ConfigOverrides::default(),
            auto_approve_packs: false,
            vmm: crate::vm::load_backend(crate::settings::Vmm::HOST_DEFAULT).unwrap(),
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
        sandbox::disk_id(&PinnedDir::pin(&self.sandbox_dir()).unwrap())
    }
}

/// The text of the setup wizard view of `form`, without colors.
pub fn view_text(form: &crate::sandbox::start::wizard::form::Form) -> String {
    let frame = crate::sandbox::start::wizard::view::frame(form, 200);
    console::strip_ansi_codes(&frame.lines.join("\n")).to_string()
}
