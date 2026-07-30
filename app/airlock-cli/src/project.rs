//! Sandbox identity, locking, and metadata.
//!
//! Each project directory that runs `airlock up` gets a `.airlock/sandbox/`
//! directory created next to the config file. This directory stores the CA
//! keypair, lock file, overlay state, and run metadata.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::config::Config;
use crate::vault::Vault;

/// A resolved project: its working directory, sandbox paths, config, and CA.
pub struct Project {
    /// `.airlock/` under the project root (holds `.gitignore` and `sandbox/`).
    pub cache_dir: PathBuf,
    /// `.airlock/sandbox/` — CA, overlay, disk image, lock, run metadata.
    pub sandbox_dir: PathBuf,
    /// Host user's home directory.
    pub host_home: PathBuf,
    /// Absolute working directory on the host.
    pub host_cwd: PathBuf,
    /// Working directory inside the container (defaults to `host_cwd`).
    pub guest_cwd: PathBuf,
    pub config: Config,
    /// CA certificate PEM (read from `ca.json` at load time).
    pub ca_cert: String,
    /// CA private key PEM (read from `ca.json` at load time).
    pub ca_key: String,
    /// True if the CA keypair was generated during this session (first run).
    pub ca_newly_generated: bool,
    /// Keyring-backed secret storage. Built lazy: no keyring I/O
    /// happens until the first `get_*`/`set_*` call, so commands that
    /// don't reference secrets never trigger an unlock prompt.
    pub vault: Vault,
    /// Held `flock` on `sandbox/lock` for the lifetime of the `Project`.
    /// Present only for locked projects (`lock()`); the kernel releases the
    /// lock when this handle drops or the process exits. Never read — kept
    /// solely as an RAII guard.
    #[allow(dead_code)]
    lock_file: Option<std::fs::File>,
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

    /// Save the last_run timestamp to `run.json` after a successful start.
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
}

/// Load project data without locking.
///
/// Resolves the project from the current working directory, loads its config,
/// and returns a `Project`. No lock is acquired and no CA is generated —
/// use this for read-only subcommands (`info`, `down`, `exec`).
///
/// `vault` is the process-global vault handle created in `main`; every
/// `Project` in one process shares the same instance so secrets loaded
/// once are reused across commands.
pub fn load(vault: Vault) -> anyhow::Result<Project> {
    let home_dir =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;
    let host_cwd = {
        let cwd = std::env::current_dir()?;
        std::fs::canonicalize(&cwd).unwrap_or(cwd)
    };
    let config = crate::config::load(&host_cwd)?;
    let cache_dir = host_cwd.join(".airlock");
    let sandbox_dir = cache_dir.join("sandbox");
    let (ca_cert, ca_key) = read_ca(&sandbox_dir).unwrap_or_default();
    let guest_cwd = read_run_meta(&sandbox_dir)
        .guest_cwd
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| host_cwd.clone());
    Ok(Project {
        cache_dir,
        sandbox_dir,
        host_home: home_dir,
        host_cwd,
        guest_cwd,
        config,
        ca_cert,
        ca_key,
        ca_newly_generated: false,
        vault,
        lock_file: None,
    })
}

/// Lock the sandbox directory and prepare it for use.
///
/// Creates `.airlock/sandbox/`, acquires a PID lockfile to prevent concurrent
/// `airlock up` runs, and generates the CA keypair if missing. The lock is
/// released when the `Project` is dropped.
///
/// `sandbox_cwd_override` sets the working directory inside the container
/// (defaults to `host_cwd` when `None`).
pub fn lock(
    host_cwd: PathBuf,
    config: Config,
    sandbox_cwd_override: Option<String>,
    vault: Vault,
) -> anyhow::Result<Project> {
    let home_dir =
        dirs::home_dir().ok_or_else(|| anyhow::anyhow!("cannot determine home directory"))?;

    let host_cwd = std::fs::canonicalize(&host_cwd).unwrap_or(host_cwd);
    let guest_cwd = sandbox_cwd_override.map_or_else(|| host_cwd.clone(), PathBuf::from);

    let cache_dir = ensure_cache_dir(&host_cwd)?;
    let sandbox_dir = cache_dir.join("sandbox");
    std::fs::create_dir_all(&sandbox_dir)?;
    // The sandbox holds the CA private key; keep other local users out of it.
    // Best-effort: the key file itself is 0600, so this is defense in depth.
    harden_dir_permissions(&sandbox_dir);
    let lock_path = sandbox_dir.join("lock");
    let lock_file = acquire_lock(&lock_path)?;

    // Persist guest_cwd in run.json so `airlock exec` can default to it.
    let mut meta = read_run_meta(&sandbox_dir);
    meta.guest_cwd = Some(guest_cwd.to_string_lossy().into_owned());
    write_run_meta(&sandbox_dir, &meta)?;

    let ca_newly_generated = !sandbox_dir.join("ca.json").exists();
    if ca_newly_generated {
        generate_ca(&sandbox_dir)?;
    }
    let (ca_cert, ca_key) = read_ca(&sandbox_dir)?;

    Ok(Project {
        cache_dir,
        sandbox_dir,
        host_home: home_dir,
        host_cwd,
        guest_cwd,
        config,
        ca_cert,
        ca_key,
        ca_newly_generated,
        vault,
        lock_file: Some(lock_file),
    })
}

// -- Private helpers --

/// Ensure `.airlock/` exists, write `.gitignore`, and return the cache dir path.
pub fn ensure_cache_dir(host_cwd: &Path) -> anyhow::Result<PathBuf> {
    let cache_dir = host_cwd.join(".airlock");
    std::fs::create_dir_all(&cache_dir)?;

    let gitignore = cache_dir.join(".gitignore");
    if !gitignore.exists() {
        std::fs::write(&gitignore, "*\n")?;
    }

    Ok(cache_dir)
}

/// Check if a project is running by probing its sandbox lock.
///
/// Attempts a non-blocking exclusive `flock` on `sandbox/lock`: if it can be
/// taken, no live process holds the lock (not running) and it is released
/// immediately when the handle drops; if it is contended, a running instance
/// holds it. This mirrors the acquisition in [`acquire_lock`] and avoids the
/// `kill(pid, 0)` pitfalls (PID reuse, `EPERM` for another user's process).
pub fn is_running(sandbox_dir: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::File::open(sandbox_dir.join("lock")) else {
        return false;
    };
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    // rc == 0 → we grabbed it → nobody was holding it → not running.
    rc != 0
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
fn acquire_lock(lock_path: &Path) -> anyhow::Result<std::fs::File> {
    use std::io::{Seek, Write};
    use std::os::unix::io::AsRawFd;

    let mut file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(lock_path)?;

    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EWOULDBLOCK) {
            let holder = std::fs::read_to_string(lock_path)
                .ok()
                .map(|s| s.trim().to_string())
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

    // We hold the lock — (re)write our PID for diagnostics.
    file.set_len(0)?;
    file.rewind()?;
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
        let lock = dir.join("lock");

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
        let _held2 = acquire_lock(&lock).expect("re-acquire after release succeeds");

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
