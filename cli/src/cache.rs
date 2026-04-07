//! Paths into the `~/.ezpez/` cache directory.

use std::path::PathBuf;

/// Root cache directory (`~/.ezpez/`), created if absent.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").map_err(|_| anyhow::anyhow!("HOME not set"))?;
    let dir = PathBuf::from(home).join(".ezpez");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for a cached OCI image, keyed by its digest hash.
pub fn image_dir(digest: &str) -> anyhow::Result<PathBuf> {
    let name = digest.split(':').next_back().unwrap_or(digest);
    let dir = cache_dir()?.join("images").join(name);
    Ok(dir)
}

/// Per-project cache directory, keyed by a hash of the project path.
pub fn project_dir(project_hash: &str) -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?.join("projects").join(project_hash);
    Ok(dir)
}
