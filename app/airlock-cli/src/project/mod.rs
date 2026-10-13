//! Project identity and sandbox data.
//!
//! Identifies the project that airlock runs in, and manages the sandbox data
//! that airlock keeps for each project. The sandbox data is in the airlock
//! data directory or in the project directory. A lock makes sure that only
//! one airlock process at a time uses the sandbox of a project. Also resolves
//! the environment variables that the project gives to the sandbox.

use std::fmt::Display;
use std::io::Write as _;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) mod sandbox_env;

pub use self::sandbox_env::{EnvError, MaskedSecret, SandboxEnv};
use crate::config::config_values::{ConfigValues, Disk};
use crate::context::Context;
use crate::messages;
use crate::sandboxes::{Found, PROJECT_SANDBOX};
use crate::util::{PinnedDir, VerifiedPath, read_to_string_capped, vfs};
use crate::vault::Vault;
use crate::vm::disk;

/// Size limit of `run.json`. The guest can write a project sandbox.
const RUN_META_CAP: u64 = 4 * 1024 * 1024;

/// Size limit of `ca.json`. The guest can write a project sandbox.
const CA_CAP: u64 = 64 * 1024;

/// A resolved project: its working directory, sandbox paths, config and CA.
///
/// It holds no lock. The caller keeps the [`SandboxLock`] for the run (see
/// [`open`]), and the lock keeps other instances out. Thus `Project` is
/// cheap to [`Clone`]. [`Self::with_config`] uses this for a boot with a
/// different config (the install boot) next to the run config of the
/// original project.
#[derive(Clone)]
pub struct Project {
    /// Sandbox directory: CA, overlay, disk image, lock, run metadata.
    pub sandbox_dir: VerifiedPath,
    /// Host user's home directory.
    pub host_home: PathBuf,
    /// Absolute working directory on the host.
    pub host_cwd: PathBuf,
    /// Working directory inside the container (default: `host_cwd`).
    pub guest_cwd: PathBuf,
    /// Resolved project config.
    pub config: ConfigValues,
    /// Resolved `[env]` section: values after host substitution, and
    /// surrogates for masked entries. Only [`open`] and
    /// [`Self::with_config`] set it. The read-only [`load`] never
    /// substitutes secrets, so there it is empty.
    pub env: SandboxEnv,
    /// CA certificate PEM (read from `ca.json` at load time).
    pub ca_cert: String,
    /// CA private key PEM (read from `ca.json` at load time).
    pub ca_key: String,
    /// Process-wide settings, vault and database. The vault opens lazily
    /// (see [`Vault`]), so commands that do not use secrets never show an
    /// unlock prompt.
    pub context: Context,
}

impl Project {
    /// Check if the sandbox data is in the project (`.airlock/sandbox`), and
    /// not in the data directory. True also if the path check fails.
    pub fn sandbox_in_project(&self) -> bool {
        // Fail closed: if the check of the path fails, treat the sandbox as
        // in the project. Then the guest still cannot see `.airlock`.
        match self.sandbox_dir.to_entry_path() {
            Ok(dir) => dir == self.host_cwd.join(PROJECT_SANDBOX),
            Err(_) => true,
        }
    }

    /// Check if an `airlock start` process runs this project now (see
    /// [`is_running`]).
    pub fn is_running(&self) -> bool {
        is_running(&self.sandbox_dir)
    }

    /// Human-readable time since the last `airlock start` run (for example
    /// "2 hours ago"). `None` if the project never ran.
    pub fn last_run_ago(&self) -> Option<String> {
        last_run_ago(&self.sandbox_dir)
    }

    /// Record a boot (the `last_run` timestamp) in `run.json`. Call it one
    /// time per VM start.
    pub fn save_meta(&self) {
        let mut meta = read_run_meta(&self.sandbox_dir);
        meta.last_run = Some(
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
        );
        let _ = write_run_meta(&self.sandbox_dir, &meta);
    }

    /// Working directory for display: the host cwd, and `host → guest` when
    /// the guest cwd is different.
    pub fn display_cwd(&self) -> String {
        if self.host_cwd == self.guest_cwd {
            self.host_cwd.display().to_string()
        } else {
            format!("{} → {}", self.host_cwd.display(), self.guest_cwd.display())
        }
    }

    /// Make a clone of this project with a different config.
    /// Args:
    ///  - `config`: Config of the clone. Its `[env]` is resolved again
    ///    (see [`resolve_env`])
    ///
    /// Returns:
    ///   Clone with the same paths, guest cwd and CA. This project does not
    ///   change. Used for the install boot next to the original project
    ///   (see [`crate::start::install::install_tools`]).
    pub fn with_config(&self, config: ConfigValues) -> Result<Self, EnvError> {
        let env = resolve_env(&config, &self.context.vault)?;
        Ok(Self {
            config,
            env,
            ..self.clone()
        })
    }
}

/// Load the project of a found sandbox without a lock. Does not make a CA
/// and does not resolve `[env]`. For read-only subcommands (`info`).
/// Args:
///  - `config`: Resolved project config
///  - `context`: Process-wide context from `main`. All projects in one
///    process share its vault, so they load secrets only one time.
///  - `found`: The sandbox of the project (see
///    [`crate::sandboxes::resolve_sandbox`])
pub fn load(config: ConfigValues, context: Context, found: &Found) -> anyhow::Result<Project> {
    let home_dir =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!(messages::project::HOME_DIR_UNKNOWN))?;
    let host_cwd = found.project.clone();
    let sandbox_dir = found.dir.clone();
    let (ca_cert, ca_key) = read_ca(&sandbox_dir).unwrap_or_default();
    let guest_cwd = read_run_meta(&sandbox_dir)
        .guest_cwd
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| host_cwd.clone());
    Ok(Project {
        sandbox_dir,
        host_home: home_dir,
        host_cwd,
        guest_cwd,
        config,
        env: SandboxEnv::empty(),
        ca_cert,
        ca_key,
        context,
    })
}

/// Held lock of the sandbox of one project (the `lock` file in the sandbox
/// directory).
///
/// Taking the lock creates the sandbox directory. The lock is released when
/// the value drops or the process exits. [`Project`] holds no lock, so the
/// caller keeps this value while the sandbox must stay exclusive (usually
/// the whole run, see [`open`]). A second lock in the same process fails
/// like a lock from a different process.
pub struct SandboxLock {
    host_cwd: PathBuf,
    sandbox_dir: VerifiedPath,
    /// Held exclusive `flock`. Never read. It is only the RAII guard that
    /// releases the lock when `SandboxLock` drops.
    #[allow(dead_code)]
    file: std::fs::File,
}

impl SandboxLock {
    /// Create the project sandbox (`.airlock/sandbox`) of the project at
    /// `host_cwd` and take its lock. Fails when a different airlock instance
    /// holds the lock.
    pub fn acquire(host_cwd: &Path) -> anyhow::Result<Self> {
        // The project can be below a symlink. Below the project root, the
        // guest can write, so no link is followed there.
        let root = VerifiedPath::new_follow(host_cwd, None)?;
        let host_cwd = root.to_path()?.to_path_buf();
        ensure_cache_dir(&host_cwd)?;
        let pinned = PinnedDir::open(&host_cwd, Path::new(PROJECT_SANDBOX), true)?;
        Self::lock(root.join(PROJECT_SANDBOX)?, host_cwd, &pinned)
    }

    /// Create the sandbox `id` in the data directory and take its lock.
    /// Fails when a different airlock instance holds the lock.
    /// Args:
    ///  - `host_cwd`: Canonical project directory
    ///  - `boxes_dir`: Sandboxes directory in the data directory
    ///  - `id`: Registry id of the sandbox
    pub fn acquire_box(
        host_cwd: &Path,
        boxes_dir: &VerifiedPath,
        id: &str,
    ) -> anyhow::Result<Self> {
        anyhow::ensure!(
            crate::sandboxes::registry::is_valid_id(id),
            messages::sandboxes::not_sandbox_id(id)
        );
        crate::cache::create_private_dir(boxes_dir)?;
        let pinned = PinnedDir::open(boxes_dir.to_path()?, Path::new(id), true)?;
        Self::lock(boxes_dir.join(id)?, host_cwd.to_path_buf(), &pinned)
    }

    /// Create the sandbox directory `sandbox_dir` that has no project, and
    /// take its lock. A template build uses this. Fails when a different
    /// airlock instance holds the lock.
    /// Args:
    ///  - `sandbox_dir`: Sandbox directory, created with mode 0700
    ///  - `host_cwd`: Directory that the sandbox uses as the project
    ///    directory
    pub fn acquire_dir(sandbox_dir: &VerifiedPath, host_cwd: &Path) -> anyhow::Result<Self> {
        crate::cache::create_private_dir(sandbox_dir)?;
        let pinned = PinnedDir::pin(sandbox_dir.to_path()?)?;
        Self::lock(sandbox_dir.clone(), host_cwd.to_path_buf(), &pinned)
    }

    /// Take the lock of the sandbox directory `pinned`.
    fn lock(
        sandbox_dir: VerifiedPath,
        host_cwd: PathBuf,
        pinned: &PinnedDir,
    ) -> anyhow::Result<Self> {
        // The sandbox holds the CA private key. Keep other local users out.
        // This is defense in depth only. The key file is always 0600.
        harden_dir_permissions(&sandbox_dir);
        let file = acquire_lock(pinned)?;
        Ok(Self {
            host_cwd,
            sandbox_dir,
            file,
        })
    }

    /// Get the sandbox directory.
    pub fn dir(&self) -> &VerifiedPath {
        &self.sandbox_dir
    }
}

/// Prepare the locked sandbox for use. Records the guest cwd, makes the CA
/// keypair if it does not exist, and resolves `[env]` (see
/// [`resolve_env`]).
/// Args:
///  - `lock`: Held sandbox lock. Gives only its paths. The caller must keep
///    the lock while the sandbox must stay exclusive (see [`SandboxLock`])
///  - `config`: Resolved project config
///  - `sandbox_cwd_override`: Working directory inside the container.
///    `None` uses the project directory
///  - `context`: Process-wide context from `main`
///
/// Returns:
///   Project without a lock. Thus a second boot from the same `lock` (see
///   [`Project::with_config`]) needs no lock of its own.
pub fn open(
    lock: &SandboxLock,
    config: ConfigValues,
    sandbox_cwd_override: Option<String>,
    context: Context,
) -> anyhow::Result<Project> {
    let home_dir =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!(messages::project::HOME_DIR_UNKNOWN))?;
    let host_cwd = lock.host_cwd.clone();
    let sandbox_dir = lock.sandbox_dir.clone();
    let guest_cwd = sandbox_cwd_override.map_or_else(|| host_cwd.clone(), PathBuf::from);

    // Store guest_cwd in run.json, so `airlock exec` can use it as default.
    let mut meta = read_run_meta(&sandbox_dir);
    meta.guest_cwd = Some(guest_cwd.to_string_lossy().into_owned());
    write_run_meta(&sandbox_dir, &meta)?;

    if !has_ca(&sandbox_dir) {
        generate_ca(&sandbox_dir)?;
    }
    let (ca_cert, ca_key) = read_ca(&sandbox_dir)?;

    // Resolve `[env]` now, under the lock but before slow steps. A missing
    // host variable or vault secret then fails here, not after an image
    // pull. Also, the network layer needs the masked secrets.
    let env = resolve_env(&config, &context.vault)?;

    Ok(Project {
        sandbox_dir,
        host_home: home_dir,
        host_cwd,
        guest_cwd,
        config,
        env,
        ca_cert,
        ca_key,
        context,
    })
}

/// Resolve the `[env]` section of a config, and check each value that an
/// enabled network rule injects.
/// Args:
///  - `config`: Project config
///  - `vault`: Vault for `${NAME}` substitution
///
/// Returns:
///   Resolved env (substituted values and surrogates for masked entries),
///   or [`EnvError`] for a missing variable or a value that a rule cannot
///   inject.
pub fn resolve_env(config: &ConfigValues, vault: &Vault) -> Result<SandboxEnv, EnvError> {
    let env = SandboxEnv::resolve(&config.env, vault)?;
    for rule in config.network.rules.values().filter(|r| r.enabled) {
        for name in &rule.inject {
            env.check_injectable(name)?;
        }
    }
    Ok(env)
}

/// Create the sandbox disk in `sandbox_dir`, or change it to the size in
/// `config` (`[disk] size`). Prints a line for each change. Use only under
/// the sandbox lock, while no VM runs. Set `copied` if the disk is a new
/// copy of a template disk.
pub fn ensure_disk(sandbox_dir: &PinnedDir, config: &Disk, copied: bool) -> anyhow::Result<()> {
    disk::ensure(sandbox_dir, config, copied)
}

/// Delete the sandbox disk in `sandbox_dir` (the stored rootfs changes and
/// the caches). The next boot creates and formats a new disk. Use only
/// under the sandbox lock, while no VM runs.
pub fn reset_disk(sandbox_dir: &PinnedDir) -> anyhow::Result<()> {
    sandbox_dir
        .remove(disk::DISK_FILE)
        .map_err(|e| anyhow::anyhow!(messages::project::remove_disk_failed(e)))?;
    sandbox_dir
        .remove(disk::DISK_ID_FILE)
        .map_err(|e| anyhow::anyhow!(messages::project::remove_disk_id_failed(e)))?;
    Ok(())
}

/// Check if the sandbox in `sandbox_dir` has its CA keypair (`ca.json`).
/// A symlink there is no CA: the next open replaces it with a new CA.
pub fn has_ca(sandbox_dir: &VerifiedPath) -> bool {
    sandbox_dir
        .join("ca.json")
        .and_then(|path| vfs::metadata(&path))
        .is_ok_and(|m| m.is_file())
}

/// Check if the sandbox in `sandbox_dir` has a disk image. Also true for
/// an image from an older airlock without an identity file. Only a
/// regular file counts. The check does not open the file, so a planted
/// FIFO cannot block it.
pub fn has_disk(sandbox_dir: &VerifiedPath) -> bool {
    sandbox_dir
        .join(disk::DISK_FILE)
        .and_then(|path| vfs::symlink_metadata(&path))
        .is_ok_and(|m| m.is_file())
}

/// Get the identity of the sandbox disk image: a random id that each new
/// image gets (see [`disk::read_id`]). `None` if there is no identity yet.
pub fn disk_id(sandbox_dir: &PinnedDir) -> Option<(u64, u64)> {
    disk::read_id(sandbox_dir)
}

// -- Helpers --

/// Make sure that `.airlock/` exists in `host_cwd` and has a `.gitignore`.
/// Returns:
///   Path of the cache dir (`.airlock/`). Error if `.airlock` is a symlink
///   or another user owns it.
pub fn ensure_cache_dir(host_cwd: &Path) -> anyhow::Result<PathBuf> {
    // Do not follow symlinks at `.airlock` and `.airlock/.gitignore` (see
    // `PinnedDir`). For a symlinked or foreign-owned `.airlock`, give a
    // clear message, not the raw `ELOOP`/`EPERM` of `PinnedDir::open`.
    let airlock = VerifiedPath::new_follow(host_cwd, None)?.join(".airlock")?;
    if let Some(problem) = airlock_dir_problem(&airlock) {
        anyhow::bail!(problem);
    }
    let cache_dir = PinnedDir::open(host_cwd, Path::new(".airlock"), true)?;
    if cache_dir.ino(".gitignore").is_none() {
        cache_dir.write_atomic(".gitignore", b"*\n", 0o644)?;
    }
    Ok(host_cwd.join(".airlock"))
}

/// Make a clear message when `path` (`.airlock`) is a symlink or another
/// user owns it. The raw `ELOOP`/`EPERM` error of `PinnedDir::open` is not
/// clear.
/// Returns:
///   `None` if `path` does not exist ([`ensure_cache_dir`] creates it) or
///   its metadata cannot be read (`PinnedDir::open` then reports it).
fn airlock_dir_problem(path: &VerifiedPath) -> Option<String> {
    let meta = vfs::symlink_metadata(path).ok()?;
    airlock_dir_problem_for(path, meta.file_type().is_symlink(), meta.uid())
}

/// Message logic of [`airlock_dir_problem`] with metadata that is already
/// read. Thus tests can run it without a real symlink or a foreign-owned
/// directory.
fn airlock_dir_problem_for(path: impl Display, is_symlink: bool, owner_uid: u32) -> Option<String> {
    if is_symlink {
        return Some(messages::project::airlock_dir_is_symlink(path));
    }
    if owner_uid != unsafe { libc::geteuid() } {
        return Some(messages::project::airlock_dir_foreign_owner(path));
    }
    None
}

/// Check if a project runs now. Tries its sandbox lock (see
/// [`lock_if_idle`]) and releases the lock immediately if it gets it.
pub fn is_running(sandbox_dir: &VerifiedPath) -> bool {
    matches!(lock_if_idle(sandbox_dir), IdleLock::Running)
}

/// State of a sandbox lock, as found by [`lock_if_idle`].
pub enum IdleLock {
    /// `sandbox/lock` does not exist: no instance ran the sandbox.
    Missing,
    /// No instance held the lock. This file holds it until it drops.
    Held(std::fs::File),
    /// A different instance holds the lock: the sandbox runs.
    Running,
}

/// Take the lock at `sandbox/lock` if no instance holds it. Unlike
/// [`SandboxLock::acquire`], it creates nothing.
///
/// `airlock rm` holds the returned lock while it removes the sandbox. Thus
/// a concurrent `airlock start` fails, and does not lose its sandbox
/// during the boot.
/// Args:
///  - `sandbox_dir`: Sandbox directory. Not followed if it is a symlink
///
/// Returns:
///   State of the lock, see [`IdleLock`].
pub fn lock_if_idle(sandbox_dir: &VerifiedPath) -> IdleLock {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    // Do not follow a symlink at `sandbox_dir` or `lock`. A committed
    // `.airlock/sandbox -> ~/.airlock/sandbox` or a `lock` symlink must not
    // give access to the lock of a different sandbox. airlock never creates
    // a symlinked `sandbox_dir`, so treat it as `Missing`.
    if vfs::symlink_metadata(sandbox_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return IdleLock::Missing;
    }
    // O_NONBLOCK: the open of a planted FIFO at `lock` must not block. The
    // regular-file check below refuses the FIFO.
    let mut opts = std::fs::OpenOptions::new();
    opts.read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    let opened = sandbox_dir
        .join("lock")
        .and_then(|lock| vfs::open_with(&lock, &opts));
    let Ok(file) = opened else {
        return IdleLock::Missing;
    };
    match file.metadata() {
        Ok(meta) if meta.is_file() => {}
        _ => return IdleLock::Missing,
    }
    // Use a non-blocking exclusive `flock`, as `acquire_lock` does. This
    // prevents the problems of `kill(pid, 0)` (PID reuse, `EPERM` for the
    // process of a different user).
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    // rc == 0: this process got the lock. No live process held it, so the
    // sandbox does not run.
    if rc == 0 {
        IdleLock::Held(file)
    } else {
        IdleLock::Running
    }
}

/// Format the last run time of the sandbox as "X ago".
/// Returns:
///   Formatted time, or `None` if `run.json` has no last run.
pub fn last_run_ago(sandbox_dir: &VerifiedPath) -> Option<String> {
    read_run_meta(sandbox_dir).last_run.map(time_ago)
}

/// Format the time since the Unix time `epoch` as "X ago".
pub fn time_ago(epoch: u64) -> String {
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let elapsed = Duration::from_secs(now.saturating_sub(epoch));
    timeago::Formatter::new().convert(elapsed)
}

/// Run metadata stored in `run.json`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
pub struct RunMeta {
    /// Unix time of the last boot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run: Option<u64>,
    /// Working directory in the guest of the last run.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guest_cwd: Option<String>,
    /// Final config of the last run (see [`crate::start::config_change`]).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_config: Option<serde_json::Value>,
    /// Resolved host paths of the last run, by config key (see
    /// [`crate::start::config_change`]). `None` in a file of an older
    /// airlock.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_run_paths: Option<HostPaths>,
}

/// Host paths of a config, by config key, for example
/// `mounts.data.source`.
pub type HostPaths = std::collections::BTreeMap<String, HostPath>;

/// A host path of the config.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct HostPath {
    /// The path as the config gives it, for example `./data`.
    pub given: String,
    /// The absolute path with its symlinks resolved.
    pub resolved: String,
}

/// Read `run.json`. Gives default metadata if the file is missing or not
/// valid.
pub fn read_run_meta(sandbox_dir: &VerifiedPath) -> RunMeta {
    sandbox_dir
        .join("run.json")
        .and_then(|path| read_to_string_capped(&path, RUN_META_CAP))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Save `config` and its resolved host paths `paths` as the final config
/// of the last run in `run.json` of `sandbox_dir`.
pub fn save_last_run_config(
    sandbox_dir: &VerifiedPath,
    config: serde_json::Value,
    paths: HostPaths,
) -> anyhow::Result<()> {
    let mut meta = read_run_meta(sandbox_dir);
    meta.last_run_config = Some(config);
    meta.last_run_paths = Some(paths);
    write_run_meta(sandbox_dir, &meta)
}

/// Write `run.json` atomically (temp file and rename).
fn write_run_meta(sandbox_dir: &VerifiedPath, meta: &RunMeta) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(meta)?;
    let tmp = sandbox_dir.join(".run.json.tmp")?;
    // Remove a stale temp file or a planted link. `create_new` never
    // follows a link that appears after the remove.
    match vfs::remove_file(&tmp) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    vfs::open_with(&tmp, &opts)?.write_all(json.as_bytes())?;
    vfs::rename(&tmp, &sandbox_dir.join("run.json")?)?;
    Ok(())
}

/// CA keypair data stored in `ca.json`.
#[derive(serde::Serialize, serde::Deserialize)]
struct CaData {
    cert: String,
    key: String,
}

/// Take the sandbox lock. The lock stays until the returned handle drops.
/// Fails if a different instance holds the lock.
///
/// The kernel also releases the lock when the process exits, even when
/// destructors do not run (for example `std::process::exit`). The file
/// contains the PID of this process, only for diagnostics.
fn acquire_lock(sandbox_dir: &PinnedDir) -> anyhow::Result<std::fs::File> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let mut file = sandbox_dir.open_append("lock", 0o644)?;

    // A non-blocking exclusive `flock` is a kernel mutex. Thus two
    // concurrent `airlock start` runs cannot both hold the sandbox.
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let holder = sandbox_dir
                .read("lock", 64)
                .ok()
                .flatten()
                .map(|bytes| String::from_utf8_lossy(&bytes).trim().to_string())
                .filter(|s| !s.is_empty());
            return match holder {
                Some(pid) => Err(anyhow::anyhow!(messages::project::sandbox_in_use_by_pid(
                    pid
                ))),
                None => Err(anyhow::anyhow!(messages::project::SANDBOX_IN_USE)),
            };
        }
        return Err(anyhow::anyhow!(messages::project::lock_failed(err)));
    }

    // This process holds the lock. Write its PID for diagnostics. The file
    // is open for append, so after the truncate the write goes to the start.
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(file)
}

/// Try to limit a directory to its owner (0700). Ignores failure (for
/// example on filesystems without Unix modes). The sensitive file in it is
/// always 0600, which is the real protection.
fn harden_dir_permissions(dir: &VerifiedPath) {
    use std::os::unix::fs::PermissionsExt;
    let _ = vfs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

/// Generate a self-signed CA keypair and write it to `ca.json`.
fn generate_ca(sandbox_dir: &VerifiedPath) -> anyhow::Result<()> {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

    let mut params = CertificateParams::new(vec![])?;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "airlock CA");

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    let ca_data = CaData {
        cert: cert.pem(),
        key: key_pair.serialize_pem(),
    };
    // ca.json holds the CA private key. Write it owner-only (0600) and
    // atomically. A truncated file after a crash blocks all later runs:
    // `has_ca` skips a new CA, but `read_ca` cannot parse the file.
    let json = serde_json::to_string_pretty(&ca_data)?;
    crate::vault::atomic_write_in(sandbox_dir, "ca.json", json.as_bytes())?;

    Ok(())
}

/// Read the CA cert and key PEM strings from `ca.json`.
fn read_ca(sandbox_dir: &VerifiedPath) -> anyhow::Result<(String, String)> {
    let json = sandbox_dir
        .join("ca.json")
        .and_then(|path| read_to_string_capped(&path, CA_CAP))
        .map_err(|_| anyhow::anyhow!(messages::project::CA_NOT_FOUND))?;
    let ca: CaData = serde_json::from_str(&json)?;
    Ok((ca.cert, ca.key))
}

#[cfg(test)]
mod tests {
    //! Tests for the project sandbox directory: the sandbox lock, symlink
    //! safety, the CA and the sandbox disk.

    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::test_cfg::{eventually, host_env_vault, temp_dir, test_context};

    /// The permission bits of `path`.
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The verified form of the test path `path`.
    fn verified(path: &Path) -> VerifiedPath {
        VerifiedPath::new_follow(path, None).unwrap()
    }

    /// Test that only one holder can have the sandbox lock, and that the
    /// running check and the idle probe follow the lock state.
    ///   1. Take the lock and check that the sandbox runs and a second take
    ///      fails
    ///   2. Release the lock, take the idle probe and check that the probe
    ///      holds the lock until it drops
    ///   3. Take the lock again and check that the idle probe says "running"
    ///   4. Release the lock and check that the sandbox does not run
    ///   5. Remove the directory and check that the probe says "missing"
    #[test]
    fn lock_is_exclusive_and_is_running_tracks_it() {
        let tmp = temp_dir();
        let path = tmp.path().join("sandbox");
        std::fs::create_dir(&path).unwrap();
        let dir = verified(&path);
        let lock = PinnedDir::pin(&path).unwrap();
        assert!(!is_running(&dir));

        let held = acquire_lock(&lock).unwrap();
        assert!(is_running(&dir));
        assert!(acquire_lock(&lock).is_err());

        drop(held);
        let probe = eventually("idle lock is taken", || match lock_if_idle(&dir) {
            IdleLock::Held(file) => Some(file),
            _ => None,
        });
        assert!(acquire_lock(&lock).is_err());
        drop(probe);

        let held2 = eventually("lock is taken", || acquire_lock(&lock).ok());
        assert!(matches!(lock_if_idle(&dir), IdleLock::Running));
        drop(held2);
        eventually("sandbox does not run", || (!is_running(&dir)).then_some(()));

        std::fs::remove_dir_all(&path).unwrap();
        assert!(matches!(lock_if_idle(&dir), IdleLock::Missing));
    }

    /// Test that the `.airlock` setup and the sandbox lock never write
    /// through a symlink, so that a hostile project cannot make airlock
    /// change host files.
    ///   1. Make `.airlock/.gitignore` and the lock file symlinks to a host
    ///      file
    ///   2. Set up `.airlock` and take the lock, and check that the host file
    ///      did not change
    ///   3. Remove the symlinks and check that the setup writes `.gitignore`
    ///      and the lock writes the process ID
    ///   4. Make `.airlock` a symlink to a directory and check the error,
    ///      that the lock fails, and that the target stays empty
    #[test]
    fn cache_dir_and_lock_do_not_follow_symlinks() {
        let tmp = temp_dir();
        let dir = tmp.path().join("project");
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "host file").unwrap();
        std::fs::create_dir_all(dir.join(".airlock/sandbox")).unwrap();
        symlink(&victim, dir.join(".airlock/.gitignore")).unwrap();
        symlink(&victim, dir.join(".airlock/sandbox/lock")).unwrap();

        // The setup does not overwrite the `.gitignore` symlink. The lock
        // refuses its symlink.
        ensure_cache_dir(&dir).unwrap();
        assert!(SandboxLock::acquire(&dir).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");

        std::fs::remove_file(dir.join(".airlock/.gitignore")).unwrap();
        std::fs::remove_file(dir.join(".airlock/sandbox/lock")).unwrap();
        ensure_cache_dir(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(".airlock/.gitignore")).unwrap(),
            "*\n"
        );
        let lock = SandboxLock::acquire(&dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join(".airlock/sandbox/lock")).unwrap(),
            std::process::id().to_string()
        );
        drop(lock);

        let other = tmp.path().join("other");
        std::fs::create_dir(&other).unwrap();
        std::fs::remove_dir_all(dir.join(".airlock")).unwrap();
        symlink(&other, dir.join(".airlock")).unwrap();
        let err = ensure_cache_dir(&dir).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "{} is a symbolic link; airlock does not follow it. Run `airlock rm` to \
                 remove just the link (its target is left untouched), or replace it with a \
                 real directory.",
                dir.join(".airlock").display()
            )
        );
        assert!(SandboxLock::acquire(&dir).is_err());
        assert!(!other.join(".gitignore").exists());
        assert!(!other.join("sandbox").exists());
    }

    /// Test that the run data and the CA never go through a symlink that
    /// the guest put in the project sandbox. A link must not leak a host
    /// file as the CA key or let airlock write a host file.
    ///   1. Make `run.json`, `ca.json` and their temp files symlinks to
    ///      host files
    ///   2. Open the project
    ///   3. Check that the host files did not change, that the CA is new,
    ///      and that `run.json` and `ca.json` are regular files
    #[test]
    fn run_data_and_ca_do_not_follow_planted_symlinks() {
        let tmp = temp_dir();
        let dir = tmp.path().join("project");
        let sandbox = dir.join(".airlock/sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        let victim = tmp.path().join("victim");
        let secret = tmp.path().join("secret");
        std::fs::write(&victim, "host file").unwrap();
        std::fs::write(&secret, r#"{"cert": "host", "key": "host"}"#).unwrap();
        symlink(&secret, sandbox.join("ca.json")).unwrap();
        symlink(&secret, sandbox.join("run.json")).unwrap();
        symlink(&victim, sandbox.join(".run.json.tmp")).unwrap();
        let ca_tmp = format!("ca.json.{}.tmp", std::process::id());
        symlink(&victim, sandbox.join(ca_tmp)).unwrap();

        let (project, _) = open_project(&dir);

        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");
        assert!(std::fs::read_to_string(&secret).unwrap().contains("host"));
        assert!(project.ca_cert.contains("BEGIN CERTIFICATE"));
        for name in ["ca.json", "run.json"] {
            let meta = std::fs::symlink_metadata(sandbox.join(name)).unwrap();
            assert!(meta.is_file(), "{name}");
        }
    }

    /// Make a FIFO at `path`.
    fn mkfifo(path: &Path) {
        use std::os::unix::ffi::OsStrExt as _;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o600) }, 0);
    }

    /// Test that a FIFO or a link that the guest put at the disk path is
    /// no disk, and that the check does not block on the FIFO.
    ///   1. Put a symlink at the disk path and check that there is no disk
    ///   2. Put a FIFO there and check that there is no disk
    #[test]
    fn planted_disk_link_or_fifo_is_no_disk() {
        let tmp = temp_dir();
        let (project, _) = open_project(tmp.path());
        let disk = project.sandbox_dir.to_path().unwrap().join(disk::DISK_FILE);
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "host file").unwrap();

        symlink(&victim, &disk).unwrap();
        assert!(!has_disk(&project.sandbox_dir));
        std::fs::remove_file(&disk).unwrap();
        mkfifo(&disk);
        assert!(!has_disk(&project.sandbox_dir));
    }

    /// Test that a FIFO that the guest put at `ca.json` does not block the
    /// open and gets a new CA, and that a directory there fails the open.
    ///   1. Put a FIFO at `ca.json` and open the project
    ///   2. Check that `ca.json` is a regular file with a new CA
    ///   3. Put a directory at `ca.json` and check that the open fails
    #[test]
    fn ca_json_fifo_is_replaced_and_ca_json_dir_fails_open() {
        let tmp = temp_dir();
        let dir = tmp.path().join("project");
        let sandbox = dir.join(".airlock/sandbox");
        std::fs::create_dir_all(&sandbox).unwrap();
        mkfifo(&sandbox.join("ca.json"));
        // Keep one lock for both opens. A fork in a parallel test can hold a
        // copy of a released lock for a short time, so a second acquire can
        // fail.
        let lock = SandboxLock::acquire(&dir).unwrap();
        let open_with_lock = || {
            let config = crate::config::config_values::parse(serde_json::json!({})).unwrap();
            open(&lock, config, None, test_context(&dir, host_env_vault(&[])))
        };

        let project = open_with_lock().unwrap();
        assert!(project.ca_cert.contains("BEGIN CERTIFICATE"));
        assert!(
            std::fs::symlink_metadata(sandbox.join("ca.json"))
                .unwrap()
                .is_file()
        );
        drop(project);

        std::fs::remove_file(sandbox.join("ca.json")).unwrap();
        std::fs::create_dir(sandbox.join("ca.json")).unwrap();
        assert!(open_with_lock().is_err());
    }

    /// Test that an `.airlock` directory of another user is refused, and that
    /// the symlink message comes first when both problems exist.
    ///   1. Check that a directory of the current user has no problem
    ///   2. Check the message for a directory of another user
    ///   3. Check that a symlink of another user gives the symlink message
    #[test]
    fn airlock_dir_owned_by_another_user_is_refused_after_symlink_check() {
        let path = Path::new("/some/.airlock");
        let euid = unsafe { libc::geteuid() };
        let other = euid.wrapping_add(1);
        let path = path.display();
        assert_eq!(airlock_dir_problem_for(&path, false, euid), None);
        assert_eq!(
            airlock_dir_problem_for(&path, false, other),
            Some("/some/.airlock is owned by another user".to_string())
        );
        assert!(
            airlock_dir_problem_for(&path, true, other)
                .unwrap()
                .contains("is a symbolic link")
        );
    }

    /// Open a project with an empty config in `dir`.
    /// Returns:
    ///   The project, and the sandbox directory pinned for disk resets.
    fn open_project(dir: &Path) -> (Project, PinnedDir) {
        let sandbox_lock = SandboxLock::acquire(dir).unwrap();
        let config = crate::config::config_values::parse(serde_json::json!({})).unwrap();
        let project = open(
            &sandbox_lock,
            config,
            None,
            test_context(dir, host_env_vault(&[])),
        )
        .unwrap();
        let pinned = PinnedDir::pin(project.sandbox_dir.to_path().unwrap()).unwrap();
        (project, pinned)
    }

    /// Test that an opened project has a private sandbox directory and CA,
    /// and records its guest working directory. Other users must not read
    /// the CA key.
    ///   1. Open a project
    ///   2. Check the modes of the sandbox directory and the CA file, and
    ///      the CA certificate
    ///   3. Save the run metadata and check the guest working directory
    #[test]
    fn opened_project_has_private_ca_and_run_metadata() {
        let tmp = temp_dir();
        let (project, _) = open_project(tmp.path());
        let dir = project.sandbox_dir.to_path().unwrap();
        assert_eq!(mode(dir), 0o700);
        assert_eq!(mode(&dir.join("ca.json")), 0o600);
        assert!(project.ca_cert.contains("BEGIN CERTIFICATE"));
        project.save_meta();
        assert!(read_run_meta(&project.sandbox_dir).guest_cwd.is_some());
    }

    /// Test that the disk identity stays the same while the disk stays, and
    /// that a reset removes the disk and its identity. The install records
    /// use the identity to find out if the disk is the same.
    ///   1. Check that a new project has no disk and no identity
    ///   2. Create the disk twice and check that the identity stays
    ///   3. Reset the disk and check that the disk and its identity are gone
    ///   4. Reset again with no disk and check that it succeeds
    #[test]
    fn disk_identity_stays_until_reset() {
        let tmp = temp_dir();
        let (project, pinned) = open_project(tmp.path());
        let path = project.sandbox_dir.to_path().unwrap().to_path_buf();
        let dir = &path;
        assert!(disk_id(&pinned).is_none());
        assert!(!has_disk(&project.sandbox_dir));

        ensure_disk(&pinned, &project.config.disk, false).unwrap();
        assert!(has_disk(&project.sandbox_dir));
        let id = disk_id(&pinned).unwrap();
        ensure_disk(&pinned, &project.config.disk, false).unwrap();
        assert_eq!(disk_id(&pinned), Some(id));

        reset_disk(&pinned).unwrap();
        assert!(disk_id(&pinned).is_none());
        assert!(!has_disk(&project.sandbox_dir));
        assert!(!dir.join(disk::DISK_ID_FILE).exists());
        reset_disk(&pinned).unwrap();
    }

    /// Test that each new disk gets a new identity, and that the identity
    /// belongs to the disk file. A new disk must not look like the old disk.
    ///   1. Make a disk with the sandbox prepare, reset it, and make a disk
    ///      again, and check that the identity changed
    ///   2. Remove the disk file and check that there is no identity
    ///   3. Make a disk and check that it has a new identity
    ///   4. Remove the identity file and check that a new disk gets one
    #[test]
    fn each_new_disk_gets_new_identity() {
        let tmp = temp_dir();
        let (project, pinned) = open_project(tmp.path());
        let path = project.sandbox_dir.to_path().unwrap().to_path_buf();
        let dir = &path;
        let prepare = || ensure_disk(&pinned, &project.config.disk, false).unwrap();

        disk::prepare(
            &project.sandbox_dir,
            &project.config.disk,
            "/root",
            Path::new("/"),
        )
        .unwrap();
        let id = disk_id(&pinned).unwrap();
        reset_disk(&pinned).unwrap();
        prepare();
        let id2 = disk_id(&pinned).unwrap();
        assert_ne!(id2, id);

        std::fs::remove_file(dir.join(disk::DISK_FILE)).unwrap();
        assert!(disk_id(&pinned).is_none());
        prepare();
        assert_ne!(disk_id(&pinned), Some(id2));

        std::fs::remove_file(dir.join(disk::DISK_ID_FILE)).unwrap();
        assert!(disk_id(&pinned).is_none());
        prepare();
        assert!(disk_id(&pinned).is_some());
    }

    /// Test that a disk reset never follows a symlink. A link at the disk
    /// path must not let a reset change a host file.
    ///   1. Replace the disk with a symlink to a host file
    ///   2. Reset the disk
    ///   3. Check that the link is gone and the host file did not change
    #[test]
    fn disk_reset_does_not_follow_symlink() {
        let tmp = temp_dir();
        let (project, pinned) = open_project(tmp.path());
        let path = project.sandbox_dir.to_path().unwrap().to_path_buf();
        let dir = &path;
        ensure_disk(&pinned, &project.config.disk, false).unwrap();

        let victim = tmp.path().join("victim");
        std::fs::write(&victim, "host file").unwrap();
        std::fs::remove_file(dir.join(disk::DISK_FILE)).unwrap();
        symlink(&victim, dir.join(disk::DISK_FILE)).unwrap();
        reset_disk(&pinned).unwrap();
        assert!(!has_disk(&project.sandbox_dir));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");
    }
}
