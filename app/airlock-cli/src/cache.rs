//! Paths into the `~/.cache/airlock/` global cache directory.
//!
//! The global cache holds VM boot assets (under `vm/`) and an `oci/` subtree
//! with extracted OCI image rootfs trees and individual OCI layer trees.
//! Per-sandbox state (CA, disk image, overlay, etc.) lives in
//! `<project>/.airlock/sandbox/` — see `sandbox.rs`.

use std::path::PathBuf;

/// On-disk format version for the per-layer cache. Bumped whenever the
/// on-disk contract changes (e.g. the extractor now sets
/// `user.overlay.opaque="x"` on parent dirs of xattr whiteouts; layers
/// produced without that mark can't be reused). Every layer dir and
/// staging file is prefixed with `{LAYER_FORMAT}.`, and the image JSON
/// schema is bumped in lockstep so stale caches are ignored instead of
/// silently poisoning fresh runs.
pub const LAYER_FORMAT: u32 = 2;

/// Shared lock for tests that mutate the process-wide `HOME` env var.
/// Any test that calls `std::env::set_var("HOME", …)` to redirect the
/// cache must hold this lock so concurrent tests don't see each other's
/// value.
#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Strip a leading `<algo>:` from a digest, returning just the hash portion.
/// `sha256:abc123…` → `abc123…`. Used as the input to [`layer_key`]; not
/// used directly as an on-disk name (the layer cache is versioned — see
/// [`LAYER_FORMAT`]).
pub fn digest_name(digest: &str) -> &str {
    digest.split(':').next_back().unwrap_or(digest)
}

/// Normalize an OCI digest into the versioned layer key used as both the
/// on-disk directory name and the identifier passed to the guest (so guest
/// mount paths match host paths). Embedding [`LAYER_FORMAT`] into every
/// layer name means a format bump automatically invalidates the old cache
/// without needing to locate and wipe it — old dirs stay around until
/// [`crate::oci::gc_sweep`] reaps them, but they're ignored by anything
/// that consults the cache.
pub fn layer_key(digest: &str) -> String {
    format!("{LAYER_FORMAT}.{}", digest_name(digest))
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
/// namespace so other cache kinds (VM assets, …) don't collide.
fn oci_root() -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?.join("oci");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Root of the image cache (`~/.cache/airlock/oci/images/`), created if
/// absent. Each entry is a single `<image-digest>` JSON file holding the
/// fully-baked `OciImage` (schema-tagged via `crate::oci::CachedImage`).
pub fn images_root() -> anyhow::Result<PathBuf> {
    let dir = oci_root()?.join("images");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Path to a cached OCI image file, keyed by its digest hash. The path may
/// or may not exist on disk — callers check.
pub fn image_path(digest: &str) -> anyhow::Result<PathBuf> {
    Ok(images_root()?.join(digest_name(digest)))
}

/// Root of the per-layer cache (`~/.cache/airlock/oci/layers/`), created if
/// absent. Each entry is `<layer-digest>/` with the layer contents extracted
/// directly at the root; the directory's presence is itself the completion
/// marker (it only appears via the atomic rename from `<layer-digest>.tmp/`).
pub fn layers_root() -> anyhow::Result<PathBuf> {
    let dir = oci_root()?.join("layers");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Directory for a single cached OCI layer, keyed by the versioned
/// [`layer_key`]. Callers holding a raw OCI digest must convert via
/// `layer_key` first; callers that read a key back from `image_layers`
/// (stored in the image JSON) pass it through unchanged.
pub fn layer_dir(key: &str) -> anyhow::Result<PathBuf> {
    Ok(layers_root()?.join(key))
}
