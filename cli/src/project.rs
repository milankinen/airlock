use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use crate::cli;
use crate::config::Config;

pub struct Project {
    pub dir: PathBuf,
    pub cwd: PathBuf,
    pub config: Config,
    pub ca_cert: PathBuf,
    pub ca_key: PathBuf,
    lock_path: PathBuf,
}

impl Drop for Project {
    fn drop(&mut self) {
        // Release the lock — only remove if it still contains our PID
        if let Ok(contents) = std::fs::read_to_string(&self.lock_path)
            && contents.trim() == std::process::id().to_string()
        {
            let _ = std::fs::remove_file(&self.lock_path);
        }
    }
}

/// Lock the project directory and prepare it for use.
///
/// Writes a PID lockfile to prevent concurrent instances from
/// modifying the same project (bundle, mounts, etc.). The lock
/// is released when the `Project` is dropped.
pub fn lock(config: Config) -> anyhow::Result<Project> {
    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    let mut hasher = Sha256::new();
    hasher.update(cwd.to_string_lossy().as_bytes());
    let hash = hex::encode(&hasher.finalize()[..16]);

    let dir = crate::cache::project_dir(&hash)?;
    std::fs::create_dir_all(&dir)?;

    let lock_path = dir.join("lock");
    acquire_lock(&lock_path)?;

    let ca_dir = dir.join("ca");
    let ca_cert = ca_dir.join("ca.crt");
    let ca_key = ca_dir.join("ca.key");

    if !ca_cert.exists() || !ca_key.exists() {
        std::fs::create_dir_all(&ca_dir)?;
        generate_ca(&ca_cert, &ca_key)?;
    }

    Ok(Project {
        dir,
        cwd,
        config,
        ca_cert,
        ca_key,
        lock_path,
    })
}

/// Acquire the project lock. Fails if another living process holds it.
fn acquire_lock(lock_path: &Path) -> anyhow::Result<()> {
    let my_pid = std::process::id().to_string();
    let mut attempts = 0;
    while attempts < 10 {
        // Try to read existing lock
        if let Ok(contents) = std::fs::read_to_string(lock_path) {
            let stored_pid = contents.trim();
            if !stored_pid.is_empty()
                && let Ok(pid) = stored_pid.parse::<i32>()
                // Check if process is still alive (signal 0 = existence check)
                && unsafe { libc::kill(pid, 0) } == 0
            {
                anyhow::bail!(
                    "another ez instance (pid {pid}) is using this project. \
                     If this is stale, remove {}",
                    lock_path.display()
                );
                // PID is dead or invalid — stale lock, take over
            }
        }

        // Write our PID atomically: write to .tmp then rename
        let tmp = lock_path.with_extension(format!("{my_pid}.tmp"));
        std::fs::write(&tmp, &my_pid)?;
        std::fs::rename(&tmp, lock_path)?;

        // Verify we won the race (another process might have written between our read and write)
        let written = std::fs::read_to_string(lock_path)?;
        if written.trim() == my_pid {
            // Locked successfully
            return Ok(());
        }
        // Lost the race — our rename succeeded but another process
        // overwrote lock_path before we verified. The tmp file was
        // already renamed away, so just retry.
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

    cli::log!("  generated project CA certificate");
    Ok(())
}
