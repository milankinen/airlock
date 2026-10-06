//! Sandbox identity, locking, and metadata.
//!
//! Each project directory that runs `airlock up` gets a `.airlock/sandbox/`
//! directory created next to the config file. This directory stores the CA
//! keypair, lock file, overlay state, and run metadata.

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

/// A resolved project: its working directory, sandbox paths, config, and CA.
///
/// Carries no lock of its own: the caller holding the [`SandboxLock`] (for
/// the lifetime of the run, see [`open`]) is what keeps other instances
/// out. `Project` is therefore cheap to [`Clone`], which [`Self::with_config`]
/// uses for a boot with a different config (the install boot) alongside
/// the run config of the project that opened it.
#[derive(Clone)]
pub struct Project {
    /// `.airlock/sandbox/` — CA, overlay, disk image, lock, run metadata.
    pub sandbox_dir: PathBuf,
    /// Host user's home directory.
    pub host_home: PathBuf,
    /// Absolute working directory on the host.
    pub host_cwd: PathBuf,
    /// Working directory inside the container (defaults to `host_cwd`).
    pub guest_cwd: PathBuf,
    pub config: ConfigValues,
    /// The resolved `[env]` section: host-substituted values plus surrogates
    /// for masked entries. Populated by [`open`] / [`Self::with_config`]
    /// only — the read-only [`load`] path never substitutes secrets, so
    /// there it is empty.
    pub env: SandboxEnv,
    /// CA certificate PEM (read from `ca.json` at load time).
    pub ca_cert: String,
    /// CA private key PEM (read from `ca.json` at load time).
    pub ca_key: String,
    /// The process-wide settings, vault and database. The vault opens
    /// lazily: no keyring I/O happens until the first `get_*`/`set_*`
    /// call, so commands that don't reference secrets never trigger an
    /// unlock prompt.
    pub context: Context,
}

impl Project {
    /// Expand `~` in `path` using the host home directory.
    pub fn expand_host_tilde(&self, path: &str) -> PathBuf {
        crate::util::expand_tilde(path, &self.host_home)
    }

    /// Check if this project has an active `airlock up` process via its PID lock.
    pub fn is_running(&self) -> bool {
        is_running(&self.sandbox_dir)
    }

    /// Human-readable time since the last `airlock up` run (e.g. "2 hours ago").
    pub fn last_run_ago(&self) -> Option<String> {
        last_run_ago(&self.sandbox_dir)
    }

    /// Record a boot in `run.json`: the `last_run` timestamp. Called once
    /// per VM start.
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
    ///
    /// Returns `(used, total)` in bytes. The disk is a sparse file so `used`
    /// is the number of allocated blocks (`blocks() * 512`) while `total` is
    /// the virtual file size. Returns `None` if the disk image does not exist.
    pub fn disk_usage(&self) -> Option<(u64, u64)> {
        use std::os::unix::fs::MetadataExt;
        let path = self.sandbox_dir.join("disk.img");
        let meta = std::fs::metadata(path).ok()?;
        Some((meta.blocks() * 512, meta.len()))
    }

    pub fn display_cwd(&self) -> String {
        if self.host_cwd == self.guest_cwd {
            self.host_cwd.display().to_string()
        } else {
            format!("{} → {}", self.host_cwd.display(), self.guest_cwd.display())
        }
    }

    /// A clone of this project with `config` instead, re-resolving its
    /// `[env]` (see [`resolve_env`]). The paths, the guest cwd and the CA
    /// stay; this project's own config is untouched. Used for a boot with
    /// a different config than the run's, alongside the original project
    /// (the install boot; see [`crate::start::install::install_tools`]).
    pub fn with_config(&self, config: ConfigValues) -> Result<Self, EnvError> {
        let env = resolve_env(&config, &self.context.vault)?;
        Ok(Self {
            config,
            env,
            ..self.clone()
        })
    }
}

/// The on-disk locations of a project's sandbox data.
pub struct ProjectPaths {
    /// `.airlock/` under the project root.
    pub cache_dir: PathBuf,
    /// `.airlock/sandbox/`.
    pub sandbox_dir: PathBuf,
}

/// Sandbox data locations for the project at `host_cwd`. Pure path
/// arithmetic: nothing is read or created, and no config is needed.
pub fn paths(host_cwd: &Path) -> ProjectPaths {
    let cache_dir = host_cwd.join(".airlock");
    let sandbox_dir = cache_dir.join("sandbox");
    ProjectPaths {
        cache_dir,
        sandbox_dir,
    }
}

/// Load project data without locking.
///
/// Resolves the project from the current working directory and returns a
/// `Project` with the resolved config `config`. No lock is acquired and no
/// CA is generated — use this for read-only subcommands (`show`).
///
/// `context` is the process-wide context created in `main`; every
/// `Project` in one process shares its vault so secrets loaded once are
/// reused across commands.
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

/// The held lock of one project's sandbox (`.airlock/sandbox/lock`).
///
/// Taking it creates `.airlock/sandbox/`. It is released when the value
/// drops or the process exits — the caller holds it for as long as the
/// sandbox must stay exclusive (typically the whole run; see [`open`]),
/// since [`Project`] itself carries no lock. A second acquisition in the
/// same process fails like one from another process.
pub struct SandboxLock {
    host_cwd: PathBuf,
    sandbox_dir: PathBuf,
    /// Held exclusive `flock`. Never read — kept solely as the RAII guard
    /// that releases the lock when `SandboxLock` drops.
    #[allow(dead_code)]
    file: std::fs::File,
}

impl SandboxLock {
    /// Create the sandbox directory of the project at `host_cwd` and take
    /// its lock. Fails when another airlock instance holds it.
    pub fn acquire(host_cwd: &Path) -> anyhow::Result<Self> {
        let host_cwd = std::fs::canonicalize(host_cwd).unwrap_or_else(|_| host_cwd.to_path_buf());
        ensure_cache_dir(&host_cwd)?;
        let sandbox_dir = host_cwd.join(".airlock/sandbox");
        let pinned = PinnedDir::open(&host_cwd, Path::new(".airlock/sandbox"), true)?;
        // The sandbox holds the CA private key; keep other local users out of
        // it. Best-effort: the key file itself is 0600, so this is defense in
        // depth.
        harden_dir_permissions(&sandbox_dir);
        let file = acquire_lock(&pinned)?;
        Ok(Self {
            host_cwd,
            sandbox_dir,
            file,
        })
    }
}

/// Prepare the sandbox of `lock` for use with `config`: record the guest
/// cwd, generate the CA keypair if missing, and resolve `[env]` (see
/// [`resolve_env`]). `lock` only lends its paths — it stays with the
/// caller, which must keep holding it for as long as the sandbox must
/// stay exclusive (see [`SandboxLock`]); the returned `Project` carries
/// none of it, so a second boot from the same `lock` ([`Project::with_config`])
/// needs no lock of its own.
///
/// `sandbox_cwd_override` sets the working directory inside the container
/// (defaults to the project directory when `None`).
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

    // Persist guest_cwd in run.json so `airlock exec` can default to it.
    let mut meta = read_run_meta(&sandbox_dir);
    meta.guest_cwd = Some(guest_cwd.to_string_lossy().into_owned());
    write_run_meta(&sandbox_dir, &meta)?;

    if !has_ca(&sandbox_dir) {
        generate_ca(&sandbox_dir)?;
    }
    let (ca_cert, ca_key) = read_ca(&sandbox_dir)?;

    // Resolve `[env]` now, while we hold the lock but before anything slow:
    // a missing host variable or vault secret fails here rather than after
    // an image pull, and the network layer needs the masked secrets.
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

/// Resolve the `[env]` of `config` through `vault` (host substitution plus
/// surrogates for masked entries), and check every value an enabled rule
/// injects. A missing variable or an uninjectable value is an
/// [`EnvError`].
pub fn resolve_env(config: &ConfigValues, vault: &Vault) -> Result<SandboxEnv, EnvError> {
    let env = SandboxEnv::resolve(&config.env, vault)?;
    for rule in config.network.rules.values().filter(|r| r.enabled) {
        for name in &rule.inject {
            env.check_injectable(name)?;
        }
    }
    Ok(env)
}

/// Create the sandbox disk at `sandbox_dir`, or bring it to the size of
/// `config` (`[disk] size`). Prints a line for each change. Only under the
/// sandbox lock, while no VM runs.
pub fn ensure_disk(sandbox_dir: &Path, config: &Disk) -> anyhow::Result<()> {
    disk::ensure(sandbox_dir, config)
}

/// Delete the sandbox disk at `sandbox_dir` (the persisted rootfs changes
/// and the caches). The next boot creates and formats a fresh one. Only
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

/// Whether the sandbox at `sandbox_dir` has its CA keypair (`ca.json`).
pub fn has_ca(sandbox_dir: &Path) -> bool {
    sandbox_dir.join("ca.json").exists()
}

/// Whether the sandbox at `sandbox_dir` has a disk image (also one from
/// an older airlock without an identity file).
pub fn has_disk(sandbox_dir: &Path) -> bool {
    sandbox_dir.join(disk::DISK_FILE).is_file()
}

/// The identity of the sandbox disk image (a random id that every new
/// image gets, see [`disk::read_id`]), or `None` when there is none yet.
pub fn disk_id(sandbox_dir: &Path) -> Option<(u64, u64)> {
    disk::read_id(sandbox_dir)
}

// -- Private helpers --

/// Ensure `.airlock/` exists, write `.gitignore`, and return the cache dir
/// path. Symlinks at `.airlock` and at `.airlock/.gitignore` are not
/// followed (see [`PinnedDir`]); a symlinked or foreign-owned `.airlock`
/// itself is refused with a clear message (see [`airlock_dir_problem`])
/// rather than [`PinnedDir::open`]'s raw `ELOOP`/`EPERM` error.
pub fn ensure_cache_dir(host_cwd: &Path) -> anyhow::Result<PathBuf> {
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

/// A clear message when `path` (`.airlock`) is a symlink or owned by
/// another user, instead of `PinnedDir::open`'s raw `ELOOP`/`EPERM` error.
/// `None` when `path` is missing (created by [`ensure_cache_dir`]) or its
/// metadata cannot be read (let `PinnedDir::open` report that).
fn airlock_dir_problem(path: &Path) -> Option<String> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    airlock_dir_problem_for(path, meta.file_type().is_symlink(), meta.uid())
}

/// The message logic of [`airlock_dir_problem`], with the metadata already
/// read, so it can run without a real symlink or foreign-owned directory.
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

/// Check if a project is running by probing its sandbox lock
/// ([`lock_if_idle`]). A lock taken by the probe is released immediately.
pub fn is_running(sandbox_dir: &Path) -> bool {
    matches!(lock_if_idle(sandbox_dir), IdleLock::Running)
}

/// The state of a sandbox lock, probed by [`lock_if_idle`].
pub enum IdleLock {
    /// `sandbox/lock` does not exist: no instance has run the sandbox.
    Missing,
    /// No instance held the lock; this file holds it until it drops.
    Held(std::fs::File),
    /// Another instance holds the lock: the sandbox is running.
    Running,
}

/// Take the lock at `sandbox/lock` if no instance holds it.
///
/// Attempts a non-blocking exclusive `flock` on `sandbox/lock`: if it can be
/// taken, no live process holds the lock (not running); if it is contended,
/// a running instance holds it. This mirrors the acquisition in
/// [`acquire_lock`] and avoids the `kill(pid, 0)` pitfalls (PID reuse,
/// `EPERM` for another user's process). Unlike [`SandboxLock::acquire`],
/// nothing is created. `airlock rm` holds the returned lock while it removes
/// the sandbox, so a concurrent `airlock start` fails instead of losing its
/// sandbox mid-boot.
///
/// Neither `sandbox_dir` nor `lock` is followed if it is a symlink — a
/// committed `.airlock/sandbox -> ~/.airlock/sandbox`, or a `lock` symlink
/// planted inside a real sandbox dir, must not make this reach into
/// another sandbox's lock (`flock` on it, or worse, blocking on a planted
/// FIFO). A symlinked `sandbox_dir` is never one this process created, so
/// it is treated as `Missing` rather than resolved.
pub fn lock_if_idle(sandbox_dir: &Path) -> IdleLock {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    if std::fs::symlink_metadata(sandbox_dir).is_ok_and(|m| m.file_type().is_symlink()) {
        return IdleLock::Missing;
    }
    // O_NONBLOCK: opening a planted FIFO at `lock` must not block; the
    // regular-file check below refuses it.
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
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    // rc == 0 → we grabbed it → nobody was holding it → not running.
    if rc == 0 {
        IdleLock::Held(file)
    } else {
        IdleLock::Running
    }
}

/// Format the last run time as "X ago".
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

/// Run metadata persisted to `run.json`.
#[derive(serde::Serialize, serde::Deserialize, Default)]
struct RunMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    last_run: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    guest_cwd: Option<String>,
}

fn read_run_meta(sandbox_dir: &Path) -> RunMeta {
    std::fs::read_to_string(sandbox_dir.join("run.json"))
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

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

/// Acquire the sandbox lock, held for the lifetime of the returned handle.
///
/// Takes a non-blocking exclusive `flock` on `sandbox/lock` — a real kernel
/// mutex — so two concurrent `airlock up` runs cannot both believe they hold
/// the sandbox (the previous write-then-verify scheme let a `rename` clobber
/// win the race for both). The lock is released automatically when the handle
/// drops, and by the kernel on process exit even when destructors are skipped
/// (e.g. `std::process::exit`). The file's contents are our PID, kept purely
/// for diagnostics.
fn acquire_lock(sandbox_dir: &PinnedDir) -> anyhow::Result<std::fs::File> {
    use std::io::Write;
    use std::os::unix::io::AsRawFd;

    let mut file = sandbox_dir.open_append("lock", 0o644)?;

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

    // We hold the lock — (re)write our PID for diagnostics (the file is
    // opened for appending, so the write lands at the new end: the start).
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    file.flush()?;
    Ok(file)
}

/// Best-effort restrict a directory to owner-only (0700). Failure is ignored
/// (e.g. filesystems without Unix modes) — the sensitive file inside is
/// written 0600 regardless, which is the real protection.
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
    // ca.json holds the CA *private key*. Write it owner-only (0600) and
    // atomically (tmp + rename) so a crash can't leave a truncated file that
    // then blocks every subsequent run (ca.json.exists() would skip
    // regeneration but read_ca would fail to parse).
    let json = serde_json::to_string_pretty(&ca_data)?;
    crate::vault::atomic_write(&sandbox_dir.join("ca.json"), json.as_bytes())?;

    Ok(())
}

/// Read the CA cert and key PEM strings from `ca.json`.
fn read_ca(sandbox_dir: &Path) -> anyhow::Result<(String, String)> {
    let json = std::fs::read_to_string(sandbox_dir.join("ca.json"))
        .map_err(|_| anyhow::anyhow!("CA not found — run `airlock up` first"))?;
    let ca: CaData = serde_json::from_str(&json)?;
    Ok((ca.cert, ca.key))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "airlock-lock-test-{}-{}-{}",
            tag,
            std::process::id(),
            SystemTime::now()
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn lock_is_exclusive_and_is_running_tracks_it() {
        let dir = scratch_dir("excl");
        let lock = PinnedDir::pin(&dir).unwrap();

        // No lock file yet → not running.
        assert!(!is_running(&dir));

        let held = acquire_lock(&lock).expect("first acquire succeeds");
        // Contended → reported as running.
        assert!(is_running(&dir));
        // A second acquisition (independent fd) must be refused, even from
        // the same process — this is the mutual-exclusion the old scheme lost.
        assert!(acquire_lock(&lock).is_err());

        drop(held);
        // Released → not running, and a fresh acquisition succeeds.
        assert!(!is_running(&dir));
        let held2 = acquire_lock(&lock).expect("re-acquire after release succeeds");
        assert!(matches!(lock_if_idle(&dir), IdleLock::Running));
        drop(held2);

        // An idle lock taken by the probe keeps other instances out until
        // it drops.
        let IdleLock::Held(probe) = lock_if_idle(&dir) else {
            panic!("an idle lock is taken");
        };
        assert!(acquire_lock(&lock).is_err());
        drop(probe);
        assert!(acquire_lock(&lock).is_ok());

        let _ = std::fs::remove_dir_all(&dir);
        assert!(matches!(lock_if_idle(&dir), IdleLock::Missing));
    }

    /// `.airlock/.gitignore` and `sandbox/lock` never follow a planted
    /// symlink, and a symlinked `.airlock` is refused.
    #[test]
    fn cache_dir_and_lock_do_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let dir = scratch_dir("symlinks");
        let victim = dir.join("victim");
        std::fs::write(&victim, "host file").unwrap();
        std::fs::create_dir_all(dir.join(".airlock/sandbox")).unwrap();
        symlink(&victim, dir.join(".airlock/.gitignore")).unwrap();
        symlink(&victim, dir.join(".airlock/sandbox/lock")).unwrap();

        // The planted `.gitignore` link is kept, not written through.
        ensure_cache_dir(&dir).unwrap();
        assert!(SandboxLock::acquire(&dir).is_err());
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");

        // A missing `.gitignore` is written.
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

        let other = scratch_dir("symlinks-other");
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
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other);
    }

    /// A symlink is refused before the owner is checked; a foreign owner
    /// of a real directory is refused too; the current user's own
    /// directory has no problem.
    #[test]
    fn airlock_dir_problem_flags_symlinks_and_foreign_owners() {
        let path = Path::new("/some/.airlock");
        let euid = unsafe { libc::geteuid() };
        assert_eq!(
            airlock_dir_problem_for(path, true, euid),
            Some(
                "/some/.airlock is a symbolic link; airlock does not follow it. Run `airlock \
                 rm` to remove just the link (its target is left untouched), or replace it \
                 with a real directory."
                    .to_string()
            )
        );
        assert_eq!(
            airlock_dir_problem_for(path, false, euid),
            None,
            "the current user's own directory has no problem"
        );
        assert_eq!(
            airlock_dir_problem_for(path, false, euid.wrapping_add(1)),
            Some("/some/.airlock is owned by another user".to_string())
        );
        // A symlink owned by someone else is still reported as a symlink.
        assert!(
            airlock_dir_problem_for(path, true, euid.wrapping_add(1))
                .unwrap()
                .contains("is a symbolic link")
        );
    }

    fn empty_config() -> ConfigValues {
        crate::config::config_values::parse(serde_json::json!({})).unwrap()
    }

    /// A context in `home` with a disabled vault that substitutes
    /// `HOST_VAR`.
    fn test_context(home: &Path) -> Context {
        crate::test_support::test_context(
            home,
            Vault::new_with(
                Box::new(crate::vault::DisabledStorage),
                std::collections::HashMap::from([("HOST_VAR".to_string(), "host".to_string())]),
                crate::vault::VaultStorageType::Disabled,
            ),
        )
    }

    #[test]
    fn the_disk_can_be_reset() {
        let dir = scratch_dir("disk");
        let sandbox_lock = SandboxLock::acquire(&dir).unwrap();
        let project = open(&sandbox_lock, empty_config(), None, test_context(&dir)).unwrap();
        let pinned = PinnedDir::pin(&project.sandbox_dir).unwrap();
        project.save_meta();
        // The guest cwd written by `open` survives the run meta updates.
        assert!(read_run_meta(&project.sandbox_dir).guest_cwd.is_some());

        assert!(disk_id(&project.sandbox_dir).is_none());
        assert!(!has_disk(&project.sandbox_dir));
        let prepare = || ensure_disk(&project.sandbox_dir, &project.config.disk).unwrap();
        prepare();
        assert!(has_disk(&project.sandbox_dir));
        let id = disk_id(&project.sandbox_dir).unwrap();
        // Booting again keeps the disk and its identity.
        prepare();
        assert_eq!(disk_id(&project.sandbox_dir), Some(id));
        reset_disk(&pinned).unwrap();
        assert!(disk_id(&project.sandbox_dir).is_none());
        assert!(!has_disk(&project.sandbox_dir));
        assert!(!project.sandbox_dir.join(disk::DISK_ID_FILE).exists());
        reset_disk(&pinned).unwrap();
        // The VM boot creates a missing disk.
        disk::prepare(
            &project.sandbox_dir,
            &project.config.disk,
            "/root",
            Path::new("/"),
        )
        .unwrap();
        assert!(disk_id(&project.sandbox_dir).is_some());
        reset_disk(&pinned).unwrap();
        // A re-created disk has a new identity, also when it gets the old
        // inode back.
        prepare();
        let id2 = disk_id(&project.sandbox_dir).unwrap();
        assert_ne!(id2, id);
        // Deleted by hand, the id file left behind: a new identity too.
        std::fs::remove_file(project.sandbox_dir.join(disk::DISK_FILE)).unwrap();
        assert!(disk_id(&project.sandbox_dir).is_none());
        prepare();
        assert_ne!(disk_id(&project.sandbox_dir), Some(id2));
        // A disk without an id file (older airlock) gets one.
        std::fs::remove_file(project.sandbox_dir.join(disk::DISK_ID_FILE)).unwrap();
        assert!(disk_id(&project.sandbox_dir).is_none());
        prepare();
        assert!(disk_id(&project.sandbox_dir).is_some());
        // A symlink at the disk is removed, not followed.
        let victim = dir.join("victim");
        std::fs::write(&victim, "host file").unwrap();
        std::fs::remove_file(project.sandbox_dir.join(disk::DISK_FILE)).unwrap();
        std::os::unix::fs::symlink(&victim, project.sandbox_dir.join(disk::DISK_FILE)).unwrap();
        reset_disk(&pinned).unwrap();
        assert!(!has_disk(&project.sandbox_dir));
        assert_eq!(std::fs::read_to_string(&victim).unwrap(), "host file");
        drop(project);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn ca_key_is_written_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let dir = scratch_dir("ca");
        generate_ca(&dir).expect("generate CA");
        let mode = std::fs::metadata(dir.join("ca.json"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o777,
            0o600,
            "CA private key file must be owner read/write only"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
