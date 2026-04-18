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
    lock_path: Option<PathBuf>,
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

    /// Install the sandbox CA cert into `sandbox_dir/ca/` as an extra overlayfs
    /// lowerdir. The supervisor mounts this as the highest-priority lowerdir so
    /// it overrides the base image without touching the upperdir.
    pub fn install_ca_cert(&self, image_rootfs: &Path) -> anyhow::Result<()> {
        let ca_cert = self.ca_cert.as_bytes();
        // CA overlay lives at sandbox_dir/ca/ (not overlay/ca/)
        let overlay_ca_dir = self.sandbox_dir.join("ca");

        let ca_stores = [
            "etc/ssl/certs/ca-certificates.crt", // Debian/Ubuntu/Alpine
            "etc/ssl/cert.pem",                  // Alpine/LibreSSL
            "etc/pki/tls/certs/ca-bundle.crt",   // RHEL/CentOS/Fedora
            "etc/ssl/ca-bundle.pem",             // openSUSE/SLES
            "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem", // RHEL/Fedora
        ];

        for ca_store in ca_stores {
            let dest = overlay_ca_dir.join(ca_store);
            let existing = std::fs::read(image_rootfs.join(ca_store)).unwrap_or_default();
            let mut out = existing;
            if !out.ends_with(b"\n") && !out.is_empty() {
                out.push(b'\n');
            }
            out.extend_from_slice(ca_cert);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&dest, &out)?;
        }

        Ok(())
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        if let Some(lock_path) = &self.lock_path
            && let Ok(contents) = std::fs::read_to_string(lock_path)
            && contents.trim() == std::process::id().to_string()
        {
            let _ = std::fs::remove_file(lock_path);
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
        lock_path: None,
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
    let lock_path = sandbox_dir.join("lock");
    acquire_lock(&lock_path)?;

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
        lock_path: Some(lock_path),
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

/// Check if a project is running by examining its lock file.
pub fn is_running(sandbox_dir: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(sandbox_dir.join("lock")) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<i32>() else {
        return false;
    };
    unsafe { libc::kill(pid, 0) == 0 }
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

/// Atomic PID lock acquisition via write-then-verify, retrying up to 10 times.
fn acquire_lock(lock_path: &Path) -> anyhow::Result<()> {
    let my_pid = std::process::id().to_string();
    let mut attempts = 0;
    while attempts < 10 {
        if let Ok(contents) = std::fs::read_to_string(lock_path) {
            let stored_pid = contents.trim();
            if !stored_pid.is_empty()
                && let Ok(pid) = stored_pid.parse::<i32>()
                && unsafe { libc::kill(pid, 0) } == 0
            {
                anyhow::bail!(
                    "another airlock instance (pid {pid}) is using this sandbox. \
                     If this is stale, remove {}",
                    lock_path.display()
                );
            }
        }

        let tmp = lock_path.with_extension(format!("{my_pid}.tmp"));
        std::fs::write(&tmp, &my_pid)?;
        std::fs::rename(&tmp, lock_path)?;

        let written = std::fs::read_to_string(lock_path)?;
        if written.trim() == my_pid {
            return Ok(());
        }
        let _ = std::fs::remove_file(&tmp);
        attempts += 1;
    }
    Err(anyhow::anyhow!("failed to obtain sandbox lock"))
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
    std::fs::write(
        sandbox_dir.join("ca.json"),
        serde_json::to_string_pretty(&ca_data)?,
    )?;

    Ok(())
}

/// Read the CA cert and key PEM strings from `ca.json`.
fn read_ca(sandbox_dir: &Path) -> anyhow::Result<(String, String)> {
    let json = std::fs::read_to_string(sandbox_dir.join("ca.json"))
        .map_err(|_| anyhow::anyhow!("CA not found — run `airlock up` first"))?;
    let ca: CaData = serde_json::from_str(&json)?;
    Ok((ca.cert, ca.key))
}
