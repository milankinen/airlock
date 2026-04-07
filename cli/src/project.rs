use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use sha2::{Digest, Sha256};

use crate::config::Config;

pub struct Project {
    pub cache_dir: PathBuf,
    pub cwd: PathBuf,
    pub config: Config,
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
    pub ca_newly_generated: bool,
    lock_path: Option<PathBuf>,
}

impl Project {
    pub fn id(&self) -> String {
        project_id(&self.cache_dir)
    }

    pub fn is_running(&self) -> bool {
        is_running(&self.cache_dir)
    }

    pub fn last_run_ago(&self) -> Option<String> {
        last_run_ago(&self.cache_dir)
    }

    /// Save image + last_run metadata after a successful VM start.
    pub fn save_meta(&self) {
        save_meta(&self.cache_dir, &self.config.image);
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        // Release the lock — only remove if it still contains our PID
        if let Some(lock_path) = &self.lock_path
            && let Ok(contents) = std::fs::read_to_string(lock_path)
            && contents.trim() == std::process::id().to_string()
        {
            let _ = std::fs::remove_file(lock_path);
        }
    }
}

/// Compute a deterministic hash for a project directory path.
pub fn project_hash(cwd: &Path) -> String {
    let mut hasher = Sha256::new();
    hasher.update(cwd.to_string_lossy().as_bytes());
    hex::encode(&hasher.finalize()[..16])
}

/// Load project data without locking.
///
/// Resolves the project from an optional path/hash argument,
/// loads its config, and returns a `Project`. No lock is acquired
/// and no CA is generated — use this for read-only subcommands.
pub fn load(arg: Option<&str>) -> anyhow::Result<Project> {
    let (cwd, cache_dir) = resolve_project_dir(arg)?;
    let config = crate::config::load(&cwd)?;
    let ca_cert = cache_dir.join("ca/ca.crt");
    let ca_key = cache_dir.join("ca/ca.key");
    Ok(Project {
        cache_dir,
        cwd,
        config,
        ca_cert,
        ca_key,
        ca_newly_generated: false,
        lock_path: None,
    })
}

/// Lock the project directory and prepare it for use.
///
/// Writes a PID lockfile to prevent concurrent instances from
/// modifying the same project (bundle, mounts, etc.). The lock
/// is released when the `Project` is dropped.
pub fn lock(config: Config) -> anyhow::Result<Project> {
    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    let hash = project_hash(&cwd);

    let dir = crate::cache::project_dir(&hash)?;
    std::fs::create_dir_all(&dir)?;

    let lock_path = dir.join("lock");
    acquire_lock(&lock_path)?;

    // Write the cwd so project list can display it
    std::fs::write(dir.join("cwd"), cwd.to_string_lossy().as_ref())?;

    let ca_dir = dir.join("ca");
    let ca_cert = ca_dir.join("ca.crt");
    let ca_key = ca_dir.join("ca.key");

    let ca_newly_generated = !ca_cert.exists() || !ca_key.exists();
    if ca_newly_generated {
        std::fs::create_dir_all(&ca_dir)?;
        generate_ca(&ca_cert, &ca_key)?;
    }

    Ok(Project {
        cache_dir: dir,
        cwd,
        config,
        ca_cert,
        ca_key,
        ca_newly_generated,
        lock_path: Some(lock_path),
    })
}

// -- Path-based helpers (used by project list which iterates dirs directly) --

/// Check if a project is running by examining its lock file.
pub fn is_running(project_dir: &Path) -> bool {
    let Ok(contents) = std::fs::read_to_string(project_dir.join("lock")) else {
        return false;
    };
    let Ok(pid) = contents.trim().parse::<i32>() else {
        return false;
    };
    unsafe { libc::kill(pid, 0) == 0 }
}

/// Extract the project hash/id from the project directory name.
pub fn project_id(project_dir: &Path) -> String {
    project_dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// Minimum prefix length such that all hashes are uniquely identified, starting at 7.
pub fn min_unique_prefix_len(hashes: &[String]) -> usize {
    if hashes.len() <= 1 {
        return 7;
    }
    for len in 7..=32_usize {
        let prefixes: HashSet<&str> = hashes.iter().map(|h| &h[..len.min(h.len())]).collect();
        if prefixes.len() == hashes.len() {
            return len;
        }
    }
    32
}

/// Read the saved image name for a project dir.
pub fn image(project_dir: &Path) -> Option<String> {
    std::fs::read_to_string(project_dir.join("image"))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Format the last run time as "X ago".
pub fn last_run_ago(project_dir: &Path) -> Option<String> {
    let epoch_str = std::fs::read_to_string(project_dir.join("last_run")).ok()?;
    let epoch: u64 = epoch_str.trim().parse().ok()?;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let elapsed = Duration::from_secs(now.saturating_sub(epoch));
    let f = timeago::Formatter::new();
    Some(f.convert(elapsed))
}

// -- Private helpers --

/// Resolve a project directory from an optional path/hash argument.
fn resolve_project_dir(arg: Option<&str>) -> anyhow::Result<(PathBuf, PathBuf)> {
    match arg {
        None => resolve_from_path(None),
        Some(s) if looks_like_hash(s) => resolve_from_hash_prefix(s),
        Some(p) => resolve_from_path(Some(p)),
    }
}

fn looks_like_hash(s: &str) -> bool {
    s.len() >= 7 && s.len() <= 32 && s.chars().all(|c| c.is_ascii_hexdigit())
}

fn resolve_from_path(path: Option<&str>) -> anyhow::Result<(PathBuf, PathBuf)> {
    let cwd = if let Some(p) = path {
        let p = PathBuf::from(p);
        std::fs::canonicalize(&p).unwrap_or(p)
    } else {
        let cwd = std::env::current_dir()?;
        std::fs::canonicalize(&cwd).unwrap_or(cwd)
    };
    let hash = project_hash(&cwd);
    let project_dir = crate::cache::project_dir(&hash)?;
    Ok((cwd, project_dir))
}

fn resolve_from_hash_prefix(prefix: &str) -> anyhow::Result<(PathBuf, PathBuf)> {
    let projects_dir = crate::cache::cache_dir()?.join("projects");
    let mut matches = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&projects_dir) {
        for entry in entries.flatten() {
            if entry.file_name().to_string_lossy().starts_with(prefix) {
                matches.push(entry.path());
            }
        }
    }
    match matches.len() {
        0 => anyhow::bail!("no project found with id '{prefix}'"),
        1 => {
            let project_dir = matches.remove(0);
            let cwd = std::fs::read_to_string(project_dir.join("cwd"))
                .map_or_else(|_| project_dir.clone(), |s| PathBuf::from(s.trim()));
            Ok((cwd, project_dir))
        }
        n => anyhow::bail!("ambiguous id '{prefix}' — matches {n} projects"),
    }
}

fn save_meta(project_dir: &Path, image: &str) {
    let _ = std::fs::write(project_dir.join("image"), image);
    let epoch = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let _ = std::fs::write(project_dir.join("last_run"), epoch.to_string());
}

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
                    "another ez instance (pid {pid}) is using this project. \
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
    Err(anyhow::anyhow!("failed to obtain project lock"))
}

fn generate_ca(cert_path: &Path, key_path: &Path) -> anyhow::Result<()> {
    use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};

    let mut params = CertificateParams::new(vec![])?;
    params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "ezpez CA");

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    std::fs::write(cert_path, cert.pem())?;
    std::fs::write(key_path, key_pair.serialize_pem())?;
    Ok(())
}
