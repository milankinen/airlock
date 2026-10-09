//! Airlock data directory locations.
//!
//! Gives the locations in the user's airlock data directory. All sandboxes
//! share it. It contains the database, the sandboxes that are not in their
//! project, the VM boot assets, the OCI images and layers, the host side of
//! the pack mounts, and fallback CLI sockets. The user settings can move
//! the directory.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

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

/// Check that `digest` is a SHA-256 OCI digest (`sha256:` and 64 lowercase
/// hex digits). Use it on each digest from a registry or an image export
/// before the digest names a cache entry.
pub fn check_digest(digest: &str) -> anyhow::Result<()> {
    let ok = digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
    });
    anyhow::ensure!(ok, "invalid image digest {digest:?}");
    Ok(())
}

/// Make sure that `name` is one plain file name, so that a cache entry
/// cannot point out of its cache directory.
fn check_entry_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !matches!(name, "" | "." | "..") && !name.contains(['/', '\0']),
        "invalid cache entry name {name:?}"
    );
    Ok(())
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

/// Data directory of the process, from the user settings (see
/// [`set_data_dir`]).
static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Set the data directory of the process. Call it one time, before the
/// first use. Later calls have no effect.
pub fn set_data_dir(dir: PathBuf) {
    let _ = DATA_DIR.set(dir);
}

/// Get the default data directory: `airlock` in the user data directory of
/// the platform (`~/Library/Application Support` on macOS,
/// `$XDG_DATA_HOME` or `~/.local/share` on Linux).
pub fn default_data_dir() -> anyhow::Result<PathBuf> {
    let base = dirs::data_dir().ok_or_else(|| anyhow::anyhow!("HOME not set"))?;
    Ok(base.join("airlock"))
}

/// Get the airlock data directory. Creates it (mode 0700) if it does not
/// exist.
pub fn data_dir() -> anyhow::Result<PathBuf> {
    let dir = match DATA_DIR.get() {
        Some(dir) => dir.clone(),
        None => default_data_dir()?,
    };
    create_private_dir(&dir)?;
    Ok(dir)
}

/// Move the cache of older airlock versions (`~/.cache/airlock`) to the
/// data directory `data_dir`, if the data directory does not exist yet.
/// Thus existing sandboxes keep their images and layers after an upgrade,
/// and airlock does not ask the registry again.
///
/// Returns:
///   The data directory to use. This is the old cache directory if the
///   move is not possible, for example because the two directories are on
///   different file systems.
pub fn migrate_legacy_cache(data_dir: PathBuf) -> PathBuf {
    let Some(legacy) = dirs::home_dir().map(|h| h.join(".cache").join("airlock")) else {
        return data_dir;
    };
    if !legacy.is_dir() || data_dir.exists() {
        return data_dir;
    }
    // One rename keeps the inodes. Thus running sandboxes keep their open
    // files, and the hard links to cached images stay valid.
    let moved = data_dir
        .parent()
        .map_or(Ok(()), create_private_dir)
        .and_then(|()| Ok(std::fs::rename(&legacy, &data_dir)?))
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt as _;
            // The data directory holds the sandbox data, so make it
            // private like a new one.
            let mode = std::fs::Permissions::from_mode(0o700);
            Ok(std::fs::set_permissions(&data_dir, mode)?)
        });
    match moved {
        Ok(()) => data_dir,
        Err(e) => {
            crate::cli::log!(
                "{} cannot move {} to {}: {e}. Using {}.",
                crate::cli::yellow("warning:"),
                legacy.display(),
                data_dir.display(),
                legacy.display()
            );
            legacy
        }
    }
}

/// Create `dir` and its missing parents. A new directory gets mode 0700:
/// the data directory holds the database and the sandbox data.
pub fn create_private_dir(dir: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))
}

/// Get the mount directory of the pack `name`
/// (`<data>/packs/mounts/<name>/`). Creates it if it does not exist.
///
/// The pack's `config.lua` gets it as `pack.directory`. The pack keeps the
/// host side of its mounts there (for example the agent settings and
/// credential files). All sandboxes that use the pack share it.
pub fn pack_mounts_dir(name: &str) -> anyhow::Result<PathBuf> {
    let dir = data_dir()?.join("packs").join("mounts").join(name);
    std::fs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!("cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Get the path of the CLI RPC Unix socket for the sandbox at `sandbox_dir`.
/// Creates the parent directory if necessary.
/// Returns:
///   `<sandbox_dir>/cli.sock`, or `<data>/sock/<hash>.sock` if
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
    let dir = data_dir()?.join("sock");
    std::fs::create_dir_all(&dir)?;
    Ok(dir.join(format!("{hash}.sock")))
}

/// Get the root of the OCI cache (`<data>/oci/`). Creates it if it
/// does not exist. It contains the `images/` and `layers/` subtrees. They
/// have their own namespace, so they do not collide with other cache kinds
/// (VM assets and others).
fn oci_root() -> anyhow::Result<PathBuf> {
    let dir = data_dir()?.join("oci");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Get the root of the image cache (`<data>/oci/images/`). Creates
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
    let name = digest_name(digest);
    check_entry_name(name)?;
    Ok(images_root()?.join(name))
}

/// Get the root of the per-layer cache (`<data>/oci/layers/`).
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
    check_entry_name(key)?;
    Ok(layers_root()?.join(key))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_cfg::home::TempHome;

    /// Test that the cache of an older airlock moves to the data
    /// directory, so that existing sandboxes find their images after an
    /// upgrade.
    ///   1. Fill `~/.cache/airlock` with an image and a layer
    ///   2. Move the old cache
    ///   3. Check that the data directory has the entries and is private,
    ///      and that the old directory is gone
    #[test]
    fn legacy_cache_moves_to_data_dir() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = TempHome::new();
        let legacy = home.path().join(".cache/airlock");
        std::fs::create_dir_all(legacy.join("oci/images")).unwrap();
        std::fs::create_dir_all(legacy.join("oci/layers/2.abc")).unwrap();
        std::fs::write(legacy.join("oci/images/abc"), "image").unwrap();
        let data = home.data_dir();

        assert_eq!(migrate_legacy_cache(data.clone()), data);

        assert_eq!(
            std::fs::read_to_string(data.join("oci/images/abc")).unwrap(),
            "image"
        );
        assert!(data.join("oci/layers/2.abc").is_dir());
        let mode = std::fs::metadata(&data).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
        assert!(!legacy.exists());
    }
}
