//! Airlock data directory locations.
//!
//! Gives the locations in the user's airlock data directory. All sandboxes
//! share it. It contains the database, the sandboxes that are not in their
//! project, the VM boot assets, the OCI images and layers, the host side of
//! the pack mounts, and fallback CLI sockets. The user settings can move
//! the directory.

use std::path::PathBuf;

use sha2::{Digest, Sha256};

use crate::messages;
use crate::util::{VerifiedPath, vfs};

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
    anyhow::ensure!(ok, messages::app::invalid_image_digest(digest));
    Ok(())
}

/// Make sure that `name` is one plain file name, so that a cache entry
/// cannot point out of its cache directory.
fn check_entry_name(name: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !matches!(name, "" | "." | "..") && !name.contains(['/', '\0']),
        messages::app::invalid_cache_entry_name(name)
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

/// Get the default data directory: `airlock` in the user data directory of
/// the platform (`~/Library/Application Support` on macOS,
/// `$XDG_DATA_HOME` or `~/.local/share` on Linux).
pub fn default_data_dir() -> anyhow::Result<PathBuf> {
    let base = dirs::data_dir().ok_or_else(|| anyhow::anyhow!(messages::app::HOME_NOT_SET))?;
    Ok(base.join("airlock"))
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
    let Some(legacy_path) = dirs::home_dir().map(|h| h.join(".cache").join("airlock")) else {
        return data_dir;
    };
    // The user can move the old cache and the data directory with a link.
    // A linked old cache moves as a link, so its target stays where it is.
    let (Some(cache), Ok(target)) = (
        legacy_path.parent(),
        VerifiedPath::new_follow(&data_dir, None),
    ) else {
        return data_dir;
    };
    let Ok(legacy) = VerifiedPath::new_follow(cache, None).and_then(|c| c.join("airlock")) else {
        return data_dir;
    };
    let legacy_is_dir = VerifiedPath::new_follow(&legacy_path, None)
        .and_then(|dir| vfs::metadata(&dir))
        .is_ok_and(|m| m.is_dir());
    if !legacy_is_dir || vfs::symlink_metadata(&target).is_ok() {
        return data_dir;
    }
    // One rename keeps the inodes. Thus running sandboxes keep their open
    // files, and the hard links to cached images stay valid.
    let moved = data_dir
        .parent()
        .map_or(Ok(()), |parent| {
            create_private_dir(&VerifiedPath::new_follow(parent, None)?)
        })
        .and_then(|()| Ok(vfs::rename(&legacy, &target)?))
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt as _;
            // The data directory holds the sandbox data, so make it
            // private like a new one. Resolve it again: it can be a link
            // now.
            let mode = std::fs::Permissions::from_mode(0o700);
            let dir = VerifiedPath::new_follow(&data_dir, None)?;
            Ok(vfs::set_permissions(&dir, mode)?)
        });
    match moved {
        Ok(()) => data_dir,
        Err(e) => {
            crate::cli::log!(
                "{}",
                messages::app::cache_move_failed(
                    crate::cli::yellow("warning:"),
                    legacy_path.display(),
                    data_dir.display(),
                    e
                )
            );
            legacy_path
        }
    }
}

/// Create `dir` and its missing parents. A new directory gets mode 0700:
/// the data directory holds the database and the sandbox data.
pub fn create_private_dir(dir: &VerifiedPath) -> anyhow::Result<()> {
    use std::os::unix::fs::DirBuilderExt as _;
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    vfs::create_dir_with(dir, &builder)
        .map_err(|e| anyhow::anyhow!(messages::app::cannot_create_dir(dir, e)))
}

/// Make a directory in the data directory, with its missing parents.
/// Args:
///  - `data_dir`: Airlock data directory, or a directory in it
///  - `rel`: Relative path of the directory below `data_dir`, without
///    `..` components
///
/// Returns:
///   The directory, or error if a part of the path is a symlink or the
///   directory cannot be made.
fn data_subdir(data_dir: &VerifiedPath, rel: &str) -> anyhow::Result<VerifiedPath> {
    let dir = data_dir.join(rel)?;
    vfs::create_dir_all(&dir)
        .map_err(|e| anyhow::anyhow!(messages::app::cannot_create_dir(&dir, e)))?;
    Ok(dir)
}

/// Get the mount directory of the pack `name`
/// (`<data_dir>/share/all/packs/<name>/`). Creates it if it does not exist.
///
/// The pack's `config.lua` gets it as `pack.directory`. The pack keeps the
/// host side of its mounts there (for example the agent settings and
/// credential files). All sandboxes that use the pack share it.
pub fn pack_mounts_dir(data_dir: &VerifiedPath, name: &str) -> anyhow::Result<VerifiedPath> {
    data_subdir(data_dir, &format!("share/all/packs/{name}"))
}

/// Get the path of the CLI RPC Unix socket for the sandbox at `sandbox_dir`.
/// Creates the parent directory if necessary.
/// Returns:
///   `<sandbox_dir>/cli.sock`, or `<data_dir>/sock/<hash>.sock` if
///   the default path is too long for a Unix socket.
pub fn cli_sock_path(
    data_dir: &VerifiedPath,
    sandbox_dir: &VerifiedPath,
) -> anyhow::Result<VerifiedPath> {
    // `AF_UNIX` has a hard `sun_path` limit of 104 bytes on macOS (108 on
    // Linux). Deeply nested project paths can be longer. 103 is the smaller
    // limit minus the trailing NUL.
    const SUN_PATH_SAFE: usize = 103;

    let default = sandbox_dir.join(airlock_common::CLI_SOCK_FILENAME)?;
    let sandbox_dir = sandbox_dir.to_path()?;
    if sandbox_dir
        .join(airlock_common::CLI_SOCK_FILENAME)
        .as_os_str()
        .len()
        <= SUN_PATH_SAFE
    {
        return Ok(default);
    }

    // Fallback: a short, stable path from a hash of the sandbox dir. Thus
    // `airlock start` and `airlock exec` get the same path without a pointer
    // file.
    let mut hasher = Sha256::new();
    hasher.update(sandbox_dir.as_os_str().as_encoded_bytes());
    let hash = hex::encode(&hasher.finalize()[..8]);
    Ok(data_subdir(data_dir, "sock")?.join(format!("{hash}.sock"))?)
}

/// Get the root of the OCI cache (`<data_dir>/oci/`). Creates it if it
/// does not exist. It contains the `images/` and `layers/` subtrees. They
/// have their own namespace, so they do not collide with other cache kinds
/// (VM assets and others).
fn oci_root(data_dir: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    data_subdir(data_dir, "oci")
}

/// Get the root of the image cache (`<data_dir>/oci/images/`). Creates
/// it if it does not exist. Each entry is one `<image-digest>` JSON file with
/// the complete `OciImage` (with a schema tag from `crate::oci::CachedImage`).
pub fn images_root(data_dir: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    data_subdir(&oci_root(data_dir)?, "images")
}

/// Get the path of a cached OCI image file for the image `digest`. The file
/// possibly does not exist. The caller must check.
pub fn image_path(data_dir: &VerifiedPath, digest: &str) -> anyhow::Result<VerifiedPath> {
    let name = digest_name(digest);
    check_entry_name(name)?;
    Ok(images_root(data_dir)?.join(name)?)
}

/// Get the root of the per-layer cache (`<data_dir>/oci/layers/`).
/// Creates it if it does not exist.
///
/// Each entry is a `<layer-key>/` directory (see [`layer_key`]) with the
/// layer contents at its root. If the directory exists, the layer is
/// complete, because the directory appears only through the atomic rename
/// from a `<layer-key>.<...>.tmp/` staging directory.
pub fn layers_root(data_dir: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    data_subdir(&oci_root(data_dir)?, "layers")
}

/// Get the directory of one cached OCI layer.
/// Args:
///  - `data_dir`: Airlock data directory
///  - `key`: Versioned layer key. Convert a raw OCI digest with
///    [`layer_key`] first. A key from `image_layers` (in the image JSON) is
///    already a layer key.
pub fn layer_dir(data_dir: &VerifiedPath, key: &str) -> anyhow::Result<VerifiedPath> {
    check_entry_name(key)?;
    Ok(layers_root(data_dir)?.join(key)?)
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

    /// Test that an old cache that is a link to an other directory moves as
    /// a link. The cache stays on the disk where the user put it.
    ///   1. Make `~/.cache/airlock` a link to a cache directory
    ///   2. Move the old cache
    ///   3. Check that the data directory is a link to the same directory,
    ///      that the entries are there, and that the target is private
    #[test]
    fn linked_legacy_cache_moves_as_link() {
        use std::os::unix::fs::PermissionsExt as _;
        let home = TempHome::new();
        let real = home.path().join("other-disk/cache");
        std::fs::create_dir_all(real.join("oci/images")).unwrap();
        std::fs::write(real.join("oci/images/abc"), "image").unwrap();
        std::fs::create_dir_all(home.path().join(".cache")).unwrap();
        std::os::unix::fs::symlink(&real, home.path().join(".cache/airlock")).unwrap();
        let data = home.data_dir();

        assert_eq!(migrate_legacy_cache(data.clone()), data);

        assert_eq!(std::fs::read_link(&data).unwrap(), real);
        assert_eq!(
            std::fs::read_to_string(real.join("oci/images/abc")).unwrap(),
            "image"
        );
        let mode = std::fs::metadata(&real).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700);
    }
}
