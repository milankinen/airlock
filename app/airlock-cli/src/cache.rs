//! Paths into the `~/.cache/airlock/` global cache directory.
//!
//! The global cache holds the kernel, initramfs, extracted OCI image
//! rootfs trees, and individual OCI layer trees. Per-sandbox state (CA,
//! disk image, overlay, etc.) lives in `<project>/.airlock/sandbox/` —
//! see `sandbox.rs`.

use std::path::PathBuf;

/// Strip a leading `<algo>:` from a digest, returning just the hash portion.
/// `sha256:abc123…` → `abc123…`. Used as the on-disk directory name.
fn digest_name(digest: &str) -> &str {
    digest.split(':').next_back().unwrap_or(digest)
}

/// Root cache directory (`~/.cache/airlock/`), created if absent.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
    let dir = home.join(".cache").join("airlock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for a cached OCI image, keyed by its digest hash.
pub fn image_dir(digest: &str) -> anyhow::Result<PathBuf> {
    Ok(cache_dir()?.join("images").join(digest_name(digest)))
}

/// Root of the per-layer cache (`~/.cache/airlock/layers/`), created if absent.
/// Each entry is `<layer-digest>/rootfs/` plus a `.ok` completion marker.
pub fn layers_root() -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?.join("layers");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for a single cached OCI layer, keyed by its digest hash.
pub fn layer_dir(digest: &str) -> anyhow::Result<PathBuf> {
    Ok(layers_root()?.join(digest_name(digest)))
}
