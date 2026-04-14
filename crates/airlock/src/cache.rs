//! Paths into the `~/.cache/airlock/` global cache directory.
//!
//! The global cache holds the kernel, initramfs, and extracted OCI image
//! rootfs trees. Per-sandbox state (CA, disk image, overlay, etc.) lives in
//! `<project>/.airlock/sandbox/` — see `sandbox.rs`.

use std::path::PathBuf;

/// Root cache directory (`~/.cache/airlock/`), created if absent.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
    let dir = home.join(".cache").join("airlock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for a cached OCI image, keyed by its digest hash.
pub fn image_dir(digest: &str) -> anyhow::Result<PathBuf> {
    let name = digest.split(':').next_back().unwrap_or(digest);
    let dir = cache_dir()?.join("images").join(name);
    Ok(dir)
}
