//! OCI image layer extraction.
//!
//! Two forms of extraction coexist during the layer-cache rollout:
//!
//! - [`extract_layers`] applies every layer into a single merged rootfs
//!   tree, resolving whiteouts by deleting their targets. The guest mounts
//!   that tree as a single overlayfs lowerdir — the historical model.
//! - [`extract_layer_cached`] extracts one layer into its own directory in
//!   the shared layer cache, preserving whiteouts as `user.overlay.*`
//!   xattrs so the tree can stand alone as an overlayfs lowerdir. Step 3
//!   of the per-layer-cache plan switches the guest to composing overlayfs
//!   from these directly.

use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;

use crate::cache;

/// OCI whiteout marker prefix (AUFS convention, inherited by OCI).
const WHITEOUT_PREFIX: &str = ".wh.";
/// Opaque-directory whiteout filename — clears all siblings at the same path
/// in lower layers.
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

/// Extract OCI image layers in order into a merged rootfs directory. Whiteout
/// files (`.wh.<name>`) delete their target in already-extracted layers.
pub fn extract_layers(layer_files: &[&Path], rootfs: &Path) -> anyhow::Result<()> {
    std::fs::create_dir_all(rootfs)?;

    for &layer_path in layer_files {
        let file = std::fs::File::open(layer_path)?;
        let gz = GzDecoder::new(file);
        let mut archive = tar::Archive::new(gz);

        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_path_buf();
            let path_str = path.to_string_lossy();

            // Handle whiteout files (OCI layer deletion markers)
            if let Some(name) = path.file_name().and_then(|n| n.to_str())
                && let Some(target_name) = name.strip_prefix(WHITEOUT_PREFIX)
            {
                if name == OPAQUE_WHITEOUT {
                    // Opaque whiteout: delete all siblings in this directory
                    if let Some(parent) = path.parent() {
                        let target_dir = rootfs.join(parent);
                        if target_dir.exists() {
                            for child in std::fs::read_dir(&target_dir)? {
                                let child = child?;
                                let _ = std::fs::remove_dir_all(child.path());
                            }
                        }
                    }
                } else {
                    // Regular whiteout: delete the named file
                    if let Some(parent) = path.parent() {
                        let target = rootfs.join(parent).join(target_name);
                        let _ = std::fs::remove_file(&target);
                        let _ = std::fs::remove_dir_all(&target);
                    }
                }
                continue;
            }

            // Skip paths that look problematic
            if path_str.contains("..") {
                continue;
            }

            let dest = rootfs.join(&path);
            entry.unpack(&dest).ok(); // ignore individual extraction errors
        }
    }

    Ok(())
}

/// Extract a single layer tarball into the shared per-layer cache.
///
/// Atomic: writes to a temp sibling and renames into place. Idempotent:
/// returns immediately if the cache entry is already marked complete with
/// the `.ok` file. Whiteouts are preserved, not resolved:
///
/// - `.wh.<name>` becomes an empty regular file at `<name>` with a
///   `user.overlay.whiteout="y"` xattr. Overlayfs mounted with
///   `userxattr` treats that as a whiteout.
/// - `.wh..wh..opq` sets `user.overlay.opaque="y"` on the parent directory,
///   marking it as opaque in overlayfs terms.
///
/// Returns the path of the extracted `rootfs/` tree.
pub fn extract_layer_cached(digest: &str, tarball: &Path) -> anyhow::Result<PathBuf> {
    let layer_dir = cache::layer_dir(digest)?;
    let rootfs = layer_dir.join("rootfs");
    let marker = layer_dir.join(".ok");
    if marker.exists() && rootfs.is_dir() {
        return Ok(rootfs);
    }

    let parent = layer_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("layer dir has no parent"))?;
    let dir_name = layer_dir
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("layer dir has no file name"))?
        .to_string_lossy()
        .into_owned();
    let tmp = parent.join(format!("{dir_name}.tmp"));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;
    let rootfs_tmp = tmp.join("rootfs");
    std::fs::create_dir_all(&rootfs_tmp)?;

    let file = std::fs::File::open(tarball)?;
    let gz = GzDecoder::new(file);
    let mut archive = tar::Archive::new(gz);

    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_path_buf();
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            continue;
        }

        if let Some(name) = path.file_name().and_then(|n| n.to_str())
            && let Some(target_name) = name.strip_prefix(WHITEOUT_PREFIX)
        {
            let parent = path.parent().unwrap_or_else(|| Path::new(""));
            if name == OPAQUE_WHITEOUT {
                let dir = rootfs_tmp.join(parent);
                std::fs::create_dir_all(&dir)?;
                xattr::set(&dir, "user.overlay.opaque", b"y").map_err(|e| {
                    anyhow::anyhow!(
                        "set user.overlay.opaque on {}: {e} \
                         (host filesystem must support user xattrs)",
                        dir.display()
                    )
                })?;
            } else {
                let dir = rootfs_tmp.join(parent);
                std::fs::create_dir_all(&dir)?;
                let target = dir.join(target_name);
                let _ = std::fs::remove_file(&target);
                let _ = std::fs::remove_dir_all(&target);
                std::fs::File::create(&target)?;
                xattr::set(&target, "user.overlay.whiteout", b"y").map_err(|e| {
                    anyhow::anyhow!(
                        "set user.overlay.whiteout on {}: {e} \
                         (host filesystem must support user xattrs)",
                        target.display()
                    )
                })?;
            }
            continue;
        }

        let dest = rootfs_tmp.join(&path);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        entry.unpack(&dest)?;
    }

    if layer_dir.exists() {
        std::fs::remove_dir_all(&layer_dir)?;
    }
    std::fs::rename(&tmp, &layer_dir)?;
    std::fs::File::create(&marker)?;
    Ok(rootfs)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::sync::Mutex;

    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;

    /// Tests mutate the process-wide `HOME` env var (read by `cache::cache_dir`)
    /// so they must not run in parallel.
    static HOME_LOCK: Mutex<()> = Mutex::new(());

    /// Build a tiny gzipped tar from in-memory `(path, content)` entries.
    /// Paths starting with `.wh.` represent whiteouts; content is ignored.
    fn build_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut b = tar::Builder::new(&mut gz);
            for (path, content) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                b.append_data(&mut header, path, *content).unwrap();
            }
            b.finish().unwrap();
        }
        gz.finish().unwrap()
    }

    #[test]
    fn extract_layer_cached_writes_regular_files() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let tarball = tmp.join("layer.tar.gz");
        std::fs::write(
            &tarball,
            build_tarball(&[("etc/hello", b"world"), ("bin/sh", b"#!/bin/sh\n")]),
        )
        .unwrap();

        let rootfs =
            extract_layer_cached("sha256:deadbeef1", &tarball).expect("extract should succeed");

        assert_eq!(std::fs::read(rootfs.join("etc/hello")).unwrap(), b"world");
        assert!(rootfs.join("bin/sh").exists());
        assert!(rootfs.parent().unwrap().join(".ok").exists());
    }

    #[test]
    fn extract_layer_cached_preserves_whiteout_as_xattr() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let tarball = tmp.join("layer.tar.gz");
        std::fs::write(
            &tarball,
            build_tarball(&[("etc/keep", b"k"), ("etc/.wh.gone", b"")]),
        )
        .unwrap();

        let rootfs = extract_layer_cached("sha256:deadbeef2", &tarball).unwrap();

        let whiteout = rootfs.join("etc/gone");
        assert!(whiteout.exists(), "whiteout placeholder file must exist");
        assert_eq!(std::fs::metadata(&whiteout).unwrap().len(), 0);
        let val = xattr::get(&whiteout, "user.overlay.whiteout").unwrap();
        assert_eq!(val.as_deref(), Some(b"y" as &[u8]));
        assert!(rootfs.join("etc/keep").exists());
    }

    #[test]
    fn extract_layer_cached_marks_opaque_directory() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let tarball = tmp.join("layer.tar.gz");
        std::fs::write(
            &tarball,
            build_tarball(&[("opt/app/.wh..wh..opq", b""), ("opt/app/new", b"n")]),
        )
        .unwrap();

        let rootfs = extract_layer_cached("sha256:deadbeef3", &tarball).unwrap();

        let opaque_dir = rootfs.join("opt/app");
        let val = xattr::get(&opaque_dir, "user.overlay.opaque").unwrap();
        assert_eq!(val.as_deref(), Some(b"y" as &[u8]));
        assert!(rootfs.join("opt/app/new").exists());
    }

    #[test]
    fn extract_layer_cached_is_idempotent() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let tarball = tmp.join("layer.tar.gz");
        std::fs::write(&tarball, build_tarball(&[("a", b"1")])).unwrap();

        let first = extract_layer_cached("sha256:deadbeef4", &tarball).unwrap();
        let marker = first.parent().unwrap().join(".ok");
        let mtime = std::fs::metadata(&marker).unwrap().modified().unwrap();

        // Second call should early-return without rewriting the marker.
        let second = extract_layer_cached("sha256:deadbeef4", &tarball).unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::metadata(&marker).unwrap().modified().unwrap(),
            mtime
        );
    }

    fn tempfile_dir() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "airlock-layer-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }
}
