use crate::config::Config;
use sha2::{Digest, Sha256};
use std::path::PathBuf;

pub struct Project {
    pub dir: PathBuf,
    pub hash: String,
    pub cwd: PathBuf,
    pub config: Config,
}

pub fn ensure(config: Config) -> anyhow::Result<Project> {
    let cwd = std::env::current_dir()?;
    let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);

    let mut hasher = Sha256::new();
    hasher.update(cwd.to_string_lossy().as_bytes());
    let hash = hex::encode(&hasher.finalize()[..16]);

    let dir = crate::oci::cache::project_dir(&hash)?;
    std::fs::create_dir_all(&dir)?;

    Ok(Project { dir, hash, cwd, config })
}
