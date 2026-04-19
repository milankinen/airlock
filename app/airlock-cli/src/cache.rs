//! Paths into the `~/.cache/airlock/` global cache directory.
//!
//! The global cache holds the kernel, initramfs, and an `oci/` subtree
//! with extracted OCI image rootfs trees and individual OCI layer trees.
//! Per-sandbox state (CA, disk image, overlay, etc.) lives in
//! `<project>/.airlock/sandbox/` — see `sandbox.rs`.

use std::path::PathBuf;

/// Strip a leading `<algo>:` from a digest, returning just the hash portion.
/// `sha256:abc123…` → `abc123…`. Used as the on-disk directory name and as
/// the layer identifier passed to the guest (so guest paths match host paths).
pub fn digest_name(digest: &str) -> &str {
    digest.split(':').next_back().unwrap_or(digest)
}

/// Root cache directory (`~/.cache/airlock/`), created if absent.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
    let dir = home.join(".cache").join("airlock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Root of the OCI cache (`~/.cache/airlock/oci/`), created if absent.
/// Holds the `images/` and `layers/` subtrees — kept under a dedicated
/// namespace so other cache kinds (kernel, assets, …) don't collide.
fn oci_root() -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?.join("oci");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Root of the image cache (`~/.cache/airlock/oci/images/`), created if
/// absent. Each entry is `<image-digest>/` holding `meta.json` (+ `image_config.json`).
pub fn images_root() -> anyhow::Result<PathBuf> {
    let dir = oci_root()?.join("images");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for a cached OCI image, keyed by its digest hash.
pub fn image_dir(digest: &str) -> anyhow::Result<PathBuf> {
    Ok(images_root()?.join(digest_name(digest)))
}

/// Root of the per-layer cache (`~/.cache/airlock/oci/layers/`), created if
/// absent. Each entry is `<layer-digest>/rootfs/` plus a `.ok` completion
/// marker.
pub fn layers_root() -> anyhow::Result<PathBuf> {
    let dir = oci_root()?.join("layers");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for a single cached OCI layer, keyed by its digest hash.
pub fn layer_dir(digest: &str) -> anyhow::Result<PathBuf> {
    Ok(layers_root()?.join(digest_name(digest)))
}
