//! Global cache locations.
//!
//! Gives the locations in the user's global airlock cache. All sandboxes share
//! this cache. It contains the VM boot assets, the OCI images and layers, the
//! host side of the pack mounts, and fallback CLI sockets.
//!
//! The state of each sandbox is not in the global cache. It is in the project
//! directory.

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// On-disk format version of the per-layer cache.
///
/// Increase it when the on-disk contract changes. For example, the
/// extractor sets `user.overlay.opaque="x"` on parent dirs of xattr
/// whiteouts. Layers without that mark are not usable. Each layer dir and staging file
/// has the prefix `{LAYER_FORMAT}.`. Increase the image JSON schema version
/// at the same time. Then new runs ignore stale caches and do not use bad
/// data from them.
pub const LAYER_FORMAT: u32 = 2;

/// Lock for tests that change the process-wide `HOME` env var. Each test
/// that sets `HOME` to move the cache must hold this lock. Thus tests that
/// run at the same time do not see the `HOME` of another test.
#[cfg(test)]
pub(crate) static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Remove a leading `<algo>:` from a digest.
/// Returns:
///   The hash part of the digest: `sha256:abc123…` gives `abc123…`.
///
/// It is the input of [`layer_key`]. Do not use it directly as a layer
/// directory name, because the layer cache is versioned (see
/// [`LAYER_FORMAT`]).
pub fn digest_name(digest: &str) -> &str {
    digest.split(':').next_back().unwrap_or(digest)
}

/// Convert an OCI digest to the versioned layer key.
/// Returns:
///   The layer key. It is the on-disk directory name and also the
///   identifier for the guest, so guest mount paths are the same as host
///   paths.
///
/// The key contains [`LAYER_FORMAT`]. Thus a format change makes the old
/// cache invalid without a search and delete. Old dirs stay until
/// [`crate::oci::gc_sweep`] removes them, but cache readers ignore them.
pub fn layer_key(digest: &str) -> String {
    format!("{LAYER_FORMAT}.{}", digest_name(digest))
}

/// Get the root cache directory (`~/.cache/airlock/`). Creates it if it
/// does not exist.
pub fn cache_dir() -> anyhow::Result<PathBuf> {
    let home = dirs::home_dir().ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
    let dir = home.join(".cache").join("airlock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the mount directory of the pack `name`
/// (`~/.cache/airlock/packs/mounts/<name>/`). Creates it if it does not exist.
///
/// The pack's `config.lua` gets it as `pack.directory`. The pack keeps the
/// host side of its mounts there (for example the agent settings and
/// credential files). All sandboxes that use the pack share it.
pub fn pack_mounts_dir(name: &str) -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?.join("packs").join("mounts").join(name);
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Get the path of the CLI RPC Unix socket for the sandbox at `sandbox_dir`.
/// Creates the parent directory if necessary.
/// Returns:
///   `<sandbox_dir>/cli.sock`, or `~/.cache/airlock/sock/<hash>.sock` if
///   the default path is too long for a Unix socket.
pub fn cli_sock_path(sandbox_dir: &Path) -> anyhow::Result<PathBuf> {
    // `AF_UNIX` has a hard `sun_path` limit of 104 bytes on macOS (108 on
    // Linux). Deeply nested project paths can be longer. 103 is the smaller
    // limit minus the trailing NUL.
    const SUN_PATH_SAFE: usize = 103;

    let default = sandbox_dir.join(airlock_common::CLI_SOCK_FILENAME);
    if default.as_os_str().len() <= SUN_PATH_SAFE {
        return Ok(default);
    }

    // Fallback: a short, stable path from a hash of the sandbox dir. Thus
    // `airlock start` and `airlock exec` get the same path without a pointer
    // file.
    let mut hasher = Sha256::new();
    hasher.update(sandbox_dir.as_os_str().as_encoded_bytes());
    let hash = hex::encode(&hasher.finalize()[..8]);
    let dir = cache_dir()?.join("sock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{hash}.sock")))
}

/// Get the root of the OCI cache (`~/.cache/airlock/oci/`). Creates it if it
/// does not exist. It contains the `images/` and `layers/` subtrees. They
/// have their own namespace, so they do not collide with other cache kinds
/// (VM assets and others).
fn oci_root() -> anyhow::Result<PathBuf> {
    let dir = cache_dir()?.join("oci");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the root of the image cache (`~/.cache/airlock/oci/images/`). Creates
/// it if it does not exist. Each entry is one `<image-digest>` JSON file with
/// the complete `OciImage` (with a schema tag from `crate::oci::CachedImage`).
pub fn images_root() -> anyhow::Result<PathBuf> {
    let dir = oci_root()?.join("images");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the path of a cached OCI image file for the image `digest`. The file
/// possibly does not exist. The caller must check.
pub fn image_path(digest: &str) -> anyhow::Result<PathBuf> {
    Ok(images_root()?.join(digest_name(digest)))
}

/// Get the root of the per-layer cache (`~/.cache/airlock/oci/layers/`).
/// Creates it if it does not exist.
///
/// Each entry is a `<layer-key>/` directory (see [`layer_key`]) with the
/// layer contents at its root. If the directory exists, the layer is
/// complete, because the directory appears only through the atomic rename
/// from a `<layer-key>.<...>.tmp/` staging directory.
pub fn layers_root() -> anyhow::Result<PathBuf> {
    let dir = oci_root()?.join("layers");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the directory of one cached OCI layer.
/// Args:
///  - `key`: Versioned layer key. Convert a raw OCI digest with
///    [`layer_key`] first. A key from `image_layers` (in the image JSON) is
///    already a layer key.
pub fn layer_dir(key: &str) -> anyhow::Result<PathBuf> {
    Ok(layers_root()?.join(key))
}
