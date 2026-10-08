//! Project identity and sandbox data.
//!
//! Identifies the project that airlock runs in, and manages the sandbox data
//! that airlock keeps for each project. A lock makes sure that only one
//! airlock process at a time uses the sandbox of a project. Also resolves the
//! environment variables that the project gives to the sandbox.

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

pub(crate) mod sandbox_env;

pub use self::sandbox_env::{EnvError, MaskedSecret, SandboxEnv};
use crate::config::config_values::{ConfigValues, Disk};
use crate::context::Context;
use crate::util::PinnedDir;
use crate::vault::Vault;
use crate::vm::disk;

/// A resolved project: its working directory, sandbox paths, config and CA.
///
/// It holds no lock. The caller keeps the [`SandboxLock`] for the run (see
/// [`open`]), and the lock keeps other instances out. Thus `Project` is
/// cheap to [`Clone`]. [`Self::with_config`] uses this for a boot with a
/// different config (the install boot) next to the run config of the
/// original project.
#[derive(Clone)]
pub struct Project {
    /// `.airlock/sandbox/`: CA, overlay, disk image, lock, run metadata.
    pub sandbox_dir: PathBuf,
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
    /// Expand `~` in `path` using the host home directory.
    pub fn expand_host_tilde(&self, path: &str) -> PathBuf {
        crate::util::expand_tilde(path, &self.host_home)
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

    /// Actual and apparent size of the sandbox disk image.
    /// Returns:
    ///   `(used, total)` in bytes, or `None` if the disk image does not
    ///   exist. `used` is the allocated size and `total` is the virtual
    ///   file size.
    pub fn disk_usage(&self) -> Option<(u64, u64)> {
        // The disk is a sparse file: `blocks() * 512` gives the allocated
        // size.
        use std::os::unix::fs::MetadataExt;
        let path = self.sandbox_dir.join("disk.img");
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.blocks() * 512, meta.len()))
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

/// On-disk locations of the sandbox data of a project.
pub struct ProjectPaths {
    /// `.airlock/` under the project root.
    pub cache_dir: PathBuf,
    /// `.airlock/sandbox/`.
    pub sandbox_dir: PathBuf,
}

/// Get the sandbox data locations for the project at `host_cwd`. Only
/// joins paths: it reads and creates nothing, and needs no config.
pub fn paths(host_cwd: &Path) -> ProjectPaths {
    let cache_dir = host_cwd.join(".airlock");
    let sandbox_dir = cache_dir.join("sandbox");
    ProjectPaths {
        cache_dir,
        sandbox_dir,
    }
}

/// Load the project of the current working directory without a lock. Does
/// not make a CA and does not resolve `[env]`. For read-only subcommands
/// (`show`).
/// Args:
///  - `config`: Resolved project config
///  - `context`: Process-wide context from `main`. All projects in one
///    process share its vault, so they load secrets only one time.
pub fn load(config: ConfigValues, context: Context) -> anyhow::Result<Project> {
    let home_dir =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let host_cwd = {
        let cwd = std::env::current_dir()?;
        std::fs::canonicalize(&cwd).unwrap_or(cwd)
    };
    let ProjectPaths { sandbox_dir, .. } = paths(&host_cwd);
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

/// Held lock of the sandbox of one project (`.airlock/sandbox/lock`).
///
/// Taking the lock creates `.airlock/sandbox/`. The lock is released when
/// the value drops or the process exits. [`Project`] holds no lock, so the
/// caller keeps this value while the sandbox must stay exclusive (usually
/// the whole run, see [`open`]). A second lock in the same process fails
/// like a lock from a different process.
pub struct SandboxLock {
    host_cwd: PathBuf,
    sandbox_dir: PathBuf,
    /// Held exclusive `flock`. Never read. It is only the RAII guard that
    /// releases the lock when `SandboxLock` drops.
    #[allow(dead_code)]
    file: std::fs::File,
}

impl SandboxLock {
    /// Create the sandbox directory of the project at `host_cwd` and take
    /// its lock. Fails when a different airlock instance holds the lock.
    pub fn acquire(host_cwd: &Path) -> anyhow::Result<Self> {
        let host_cwd = std::fs::canonicalize(host_cwd).unwrap_or_else(|_| host_cwd.to_path_buf());
        ensure_cache_dir(&host_cwd)?;
        let sandbox_dir = host_cwd.join(".airlock/sandbox");
        let pinned = PinnedDir::open(&host_cwd, Path::new(".airlock/sandbox"), true)?;
        // The sandbox holds the CA private key. Keep other local users out.
        // This is defense in depth only. The key file is always 0600.
        harden_dir_permissions(&sandbox_dir);
        let file = acquire_lock(&pinned)?;
        Ok(Self {
            host_cwd,
            sandbox_dir,
            file,
        })
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
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
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
/// the sandbox lock, while no VM runs.
pub fn ensure_disk(sandbox_dir: &Path, config: &Disk) -> anyhow::Result<()> {
    disk::ensure(sandbox_dir, config)
}

/// Delete the sandbox disk in `sandbox_dir` (the stored rootfs changes and
/// the caches). The next boot creates and formats a new disk. Use only
/// under the sandbox lock, while no VM runs.
pub fn reset_disk(sandbox_dir: &PinnedDir) -> anyhow::Result<()> {
    sandbox_dir
        .remove(disk::DISK_FILE)
        .map_err(|e| anyhow::anyhow!("remove sandbox disk: {e}"))?;
    sandbox_dir
        .remove(disk::DISK_ID_FILE)
        .map_err(|e| anyhow::anyhow!("remove sandbox disk id: {e}"))?;
    Ok(())
}

/// Check if the sandbox in `sandbox_dir` has its CA keypair (`ca.json`).
pub fn has_ca(sandbox_dir: &Path) -> bool {
    sandbox_dir.join("ca.json").exists()
}

/// Check if the sandbox in `sandbox_dir` has a disk image. Also true for
/// an image from an older airlock without an identity file.
pub fn has_disk(sandbox_dir: &Path) -> bool {
    sandbox_dir.join(disk::DISK_FILE).is_file()
}

/// Get the identity of the sandbox disk image: a random id that each new
/// image gets (see [`disk::read_id`]). `None` if there is no identity yet.
pub fn disk_id(sandbox_dir: &Path) -> Option<(u64, u64)> {
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
    let path = host_cwd.join(".airlock");
    if let Some(problem) = airlock_dir_problem(&path) {
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
fn airlock_dir_problem(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    airlock_dir_problem_for(path, meta.file_type().is_symlink(), meta.uid())
}

/// Message logic of [`airlock_dir_problem`] with metadata that is already
/// read. Thus tests can run it without a real symlink or a foreign-owned
/// directory.
fn airlock_dir_problem_for(path: &Path, is_symlink: bool, owner_uid: u32) -> Option<String> {
    if is_symlink {
        return Some(format!(
            "{} is a symbolic link; airlock does not follow it. Run `airlock rm` to remove \
             just the link (its target is left untouched), or replace it with a real \
             directory.",
            path.display()
        ));
    }
    if owner_uid != unsafe { libc::geteuid() } {
        return Some(format!("{} is owned by another user", path.display()));
    }
    None
}

/// Check if a project runs now. Tries its sandbox lock (see
/// [`lock_if_idle`]) and releases the lock immediately if it gets it.
pub fn is_running(sandbox_dir: &Path) -> bool {
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
pub fn lock_if_idle(sandbox_dir: &Path) -> IdleLock {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    // Do not follow a symlink at `sandbox_dir` or `lock`. A committed
    // `.airlock/sandbox -> ~/.airlock/sandbox` or a `lock` symlink must not
    // give access to the lock of a different sandbox. airlock never creates
    // a symlinked `sandbox_dir`, so treat it as `Missing`.
    if std::fs::symlink_metadata(sandbox_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return IdleLock::Missing;
    }
    // O_NONBLOCK: the open of a planted FIFO at `lock` must not block. The
    // regular-file check below refuses the FIFO.
    let opened = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(sandbox_dir.join("lock"));
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
pub fn last_run_ago(sandbox_dir: &Path) -> Option<String> {
    let epoch = read_run_meta(sandbox_dir).last_run?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let elapsed = Duration::from_secs(now.saturating_sub(epoch));
    let f = timeago::Formatter::new();
    Some(f.convert(elapsed))
}

/// Run metadata stored in `run.json`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct RunMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    last_run: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guest_cwd: Option<String>,
}

/// Read `run.json`. Gives default metadata if the file is missing or not
/// valid.
fn read_run_meta(sandbox_dir: &Path) -> RunMeta {
    std::fs::read_to_string(sandbox_dir.join("run.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

/// Write `run.json` atomically (temp file and rename).
fn write_run_meta(sandbox_dir: &Path, meta: &RunMeta) -> anyhow::Result<()> {
    let json = serde_json::to_string_pretty(meta)?;
    let tmp = sandbox_dir.join(".run.json.tmp");
    std::fs::write(&tmp, &json)?;
    std::fs::rename(&tmp, sandbox_dir.join("run.json"))?;
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
                Some(pid) => Err(anyhow::anyhow!(
                    "another airlock instance (pid {pid}) is using this sandbox"
                )),
                None => Err(anyhow::anyhow!(
                    "another airlock instance is using this sandbox"
                )),
            };
        }
        return Err(anyhow::anyhow!("failed to lock sandbox: {err}"));
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
fn harden_dir_permissions(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
}

/// Generate a self-signed CA keypair and write it to `ca.json`.
fn generate_ca(sandbox_dir: &Path) -> anyhow::Result<()> {
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
    crate::vault::atomic_write(&sandbox_dir.join("ca.json"), json.as_bytes())?;

    Ok(())
}

/// Read the CA cert and key PEM strings from `ca.json`.
fn read_ca(sandbox_dir: &Path) -> anyhow::Result<(String, String)> {
    let json = std::fs::read_to_string(sandbox_dir.join("ca.json"))
        .map_err(|_| anyhow::anyhow!("CA not found — run `airlock start` first"))?;
    let ca: CaData = serde_json::from_str(&json)?;
    Ok((ca.cert, ca.key))
}

#[cfg(test)]
mod tests {
    //! Tests for the project sandbox directory: the sandbox lock, symlink
    //! safety, the CA and the sandbox disk.

    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::test_cfg::{host_env_vault, temp_dir, test_context};

    /// The permission bits of `path`.
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Test that only one holder can have the sandbox lock, and that the
    /// running check and the idle probe follow the lock state.
    ///   1. Take the lock and check that the sandbox runs and a second take
    ///      fails
    ///   2. Release the lock and check that the sandbox does not run
    ///   3. Take the lock again and check that the idle probe says "running"
    ///   4. Release it, take the idle probe and check that the probe holds
    ///      the lock until it drops
    ///   5. Remove the directory and check that the probe says "missing"
    #[test]
    fn lock_is_exclusive_and_is_running_tracks_it() {
        let tmp = temp_dir();
        let dir = tmp.path().join("sandbox");
        std::fs::create_dir(&dir).unwrap();
        let lock = PinnedDir::pin(&dir).unwrap();
        assert!(!is_running(&dir));

        let held = acquire_lock(&lock).unwrap();
        assert!(is_running(&dir));
        assert!(acquire_lock(&lock).is_err());

        drop(held);
        assert!(!is_running(&dir));
        let held2 = acquire_lock(&lock).unwrap();
        assert!(matches!(lock_if_idle(&dir), IdleLock::Running));
        drop(held2);

        let IdleLock::Held(probe) = lock_if_idle(&dir) else {
            panic!("idle lock is not taken");
        };
        assert!(acquire_lock(&lock).is_err());
        drop(probe);
        assert!(acquire_lock(&lock).is_ok());

        std::fs::remove_dir_all(&dir).unwrap();
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
        assert_eq!(airlock_dir_problem_for(path, false, euid), None);
        assert_eq!(
            airlock_dir_problem_for(path, false, other),
            Some("/some/.airlock is owned by another user".to_string())
        );
        assert!(
            airlock_dir_problem_for(path, true, other)
                .unwrap()
                .contains("is a symbolic link")
        );
    }

    /// Test that an opened project has a private CA and a sandbox disk with
    /// an identity that changes on each new disk, and that a disk reset
    /// never follows a symlink.
    ///   1. Open a project and check the modes of the sandbox directory and
    ///      the CA file, and the run metadata
    ///   2. Create the disk twice and check that the identity stays
    ///   3. Reset the disk and check that the disk and its identity are gone
    ///   4. Make new disks after a reset and after removal of the disk or
    ///      identity file, and check that each gets a new identity
    ///   5. Replace the disk with a symlink to a host file, reset, and check
    ///      that the host file did not change
    #[test]
    fn opened_project_has_private_ca_and_resettable_disk() {
        let tmp = temp_dir();
        let dir = tmp.path();
        let sandbox_lock = SandboxLock::acquire(dir).unwrap();
        let config = crate::config::config_values::parse(serde_json::json!({})).unwrap();
        let project = open(
            &sandbox_lock,
            config,
            None,
            test_context(dir, host_env_vault(&[])),
        )
        .unwrap();
        assert_eq!(mode(&project.sandbox_dir), 0o700);
        assert_eq!(mode(&project.sandbox_dir.join("ca.json")), 0o600);
        assert!(project.ca_cert.contains("BEGIN CERTIFICATE"));
        let pinned = PinnedDir::pin(&project.sandbox_dir).unwrap();
        project.save_meta();
        assert!(read_run_meta(&project.sandbox_dir).guest_cwd.is_some());

        assert!(disk_id(&project.sandbox_dir).is_none());
        assert!(!has_disk(&project.sandbox_dir));
        let prepare = || ensure_disk(&project.sandbox_dir, &project.config.disk).unwrap();
        prepare();
        assert!(has_disk(&project.sandbox_dir));
        let id = disk_id(&project.sandbox_dir).unwrap();
        prepare();
        assert_eq!(disk_id(&project.sandbox_dir), Some(id));
        reset_disk(&pinned).unwrap();
        assert!(disk_id(&project.sandbox_dir).is_none());
        assert!(!has_disk(&project.sandbox_dir));
        assert!(!project.sandbox_dir.join(disk::DISK_ID_FILE).exists());
        // A reset with no disk must also succeed.
        reset_disk(&pinned).unwrap();
        disk::prepare(
            &project.sandbox_dir,
            &project.config.disk,
            "/root",
            Path::new("/"),
        )
        .unwrap();
        assert!(disk_id(&project.sandbox_dir).is_some());
        reset_disk(&pinned).unwrap();
        prepare();
        let id2 = disk_id(&project.sandbox_dir).unwrap();
        assert_ne!(id2, id);
        // The identity belongs to the disk file. Without the disk file there
        // is no identity.
        std::fs::remove_file(project.sandbox_dir.join(disk::DISK_FILE)).unwrap();
        assert!(disk_id(&project.sandbox_dir).is_none());
        prepare();
        assert_ne!(disk_id(&project.sandbox_dir), Some(id2));
        std::fs::remove_file(project.sandbox_dir.join(disk::DISK_ID_FILE)).unwrap();
        assert!(disk_id(&project.sandbox_dir).is_none());
        prepare();
        assert!(disk_id(&project.sandbox_dir).is_some());
        let victim = dir.join("victim");
        std::fs::write(&victim, "host file").unwrap();
        std::fs::remove_file(project.sandbox_dir.join(disk::DISK_FILE)).unwrap();
        symlink(&victim, project.sandbox_dir.join(disk::DISK_FILE)).unwrap();
        reset_disk(&pinned).unwrap();
        assert!(!has_disk(&project.sandbox_dir));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");
    }
}
