//! OCI image layer download + extraction, staged through the per-layer cache.
//!
//! A layer moves through three on-disk states under
//! `~/.cache/airlock/oci/layers/`:
//!
//! ```text
//! <digest>.download.tmp   # in-flight download
//! <digest>.download       # complete tarball, pending extraction
//! <digest>.tmp/           # in-flight extraction
//! <digest>/               # finished layer tree (rename = commit)
//! ```
//!
//! Each transition is an atomic rename, so a crash at any point leaves a
//! state the next run can either clean up ([`gc::sweep`]) or resume from
//! ([`ensure_layer_cached`]).

use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use flate2::read::GzDecoder;
use indicatif::ProgressBar;

use crate::cache;

/// OCI whiteout marker prefix (AUFS convention, inherited by OCI).
const WHITEOUT_PREFIX: &str = ".wh.";
/// Opaque-directory whiteout filename — clears all siblings at the same path
/// in lower layers.
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

/// Ensure a layer is extracted into the shared cache, downloading the
/// tarball through `fetch` only if it's not already on disk.
///
/// - Fast path: `<digest>/` exists → return immediately. The directory
///   only becomes visible via the atomic rename at the end of extraction,
///   so its presence is itself the commit marker.
/// - Tarball path: if `<digest>.download` exists (from a previous run or
///   from a pre-staging caller like the docker path), skip `fetch` and
///   go straight to extraction.
/// - Otherwise: call `fetch(&tmp_path)` to write the tarball at
///   `<digest>.download.tmp`, rename to `<digest>.download`, then extract.
///
/// After a successful extraction the tarball is removed.
///
/// `progress`, when provided, is re-used as the extraction bar: its length
/// is reset to the tarball size, its position to zero, and its message to
/// `extracting` before bytes start streaming through.
pub fn ensure_layer_cached<F>(
    digest: &str,
    fetch: F,
    progress: Option<&ProgressBar>,
) -> anyhow::Result<PathBuf>
where
    F: FnOnce(&Path) -> anyhow::Result<()>,
{
    let layer_dir = cache::layer_dir(digest)?;
    if layer_dir.is_dir() {
        return Ok(layer_dir);
    }

    let parent = layer_dir
        .parent()
        .ok_or_else(|| anyhow::anyhow!("layer dir has no parent"))?;
    let dir_name = layer_dir
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("layer dir has no file name"))?
        .to_string_lossy()
        .into_owned();

    let download = parent.join(format!("{dir_name}.download"));
    let download_tmp = parent.join(format!("{dir_name}.download.tmp"));

    if !download.exists() {
        let _ = std::fs::remove_file(&download_tmp);
        fetch(&download_tmp)?;
        std::fs::rename(&download_tmp, &download)?;
    }

    extract_tarball_to_cache(&layer_dir, &download, progress)?;
    let _ = std::fs::remove_file(&download);
    if let Some(pb) = progress {
        pb.set_message("ready");
    }
    Ok(layer_dir)
}

/// Extract `tarball` into `<layer_dir>.tmp/` then atomically rename into
/// `layer_dir/`. The rename is the commit point — readers only see
/// `layer_dir/` once extraction finished cleanly. Whiteouts are preserved:
///
/// - `.wh.<name>` becomes an empty regular file at `<name>` with a
///   `user.overlay.whiteout="y"` xattr. Overlayfs mounted with
///   `userxattr` treats that as a whiteout.
/// - `.wh..wh..opq` sets `user.overlay.opaque="y"` on the parent directory,
///   marking it as opaque in overlayfs terms.
fn extract_tarball_to_cache(
    layer_dir: &Path,
    tarball: &Path,
    progress: Option<&ProgressBar>,
) -> anyhow::Result<()> {
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

    // Layer blobs may be gzip-compressed (OCI spec, registry pulls) or plain
    // tar (`docker image save` with the classic driver) — dispatch on magic.
    let file = std::fs::File::open(tarball)?;
    let file: Box<dyn Read> = match progress {
        Some(pb) => {
            let total = file.metadata().map(|m| m.len()).unwrap_or(0);
            pb.set_length(total);
            pb.set_position(0);
            pb.set_message("extracting");
            Box::new(ProgressReader {
                inner: file,
                bar: pb.clone(),
            })
        }
        None => Box::new(file),
    };
    let mut reader = BufReader::new(file);
    let mut magic = [0u8; 2];
    let n = reader.read(&mut magic)?;
    let head = std::io::Cursor::new(magic[..n].to_vec());
    let body: Box<dyn Read> = if n == 2 && magic == [0x1f, 0x8b] {
        Box::new(GzDecoder::new(head.chain(reader)))
    } else {
        Box::new(head.chain(reader))
    };
    let mut archive = tar::Archive::new(body);

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
            let parent_rel = path.parent().unwrap_or_else(|| Path::new(""));
            if name == OPAQUE_WHITEOUT {
                let dir = tmp.join(parent_rel);
                std::fs::create_dir_all(&dir)?;
                xattr::set(&dir, "user.overlay.opaque", b"y").map_err(|e| {
                    anyhow::anyhow!(
                        "set user.overlay.opaque on {}: {e} \
                         (host filesystem must support user xattrs)",
                        dir.display()
                    )
                })?;
            } else {
                let dir = tmp.join(parent_rel);
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

        // `unpack_in` resolves the entry path relative to the extraction root
        // and — critically — rewrites hardlink targets to stay inside it, so
        // `ln /absolute/host/path /extract/root/foo` never happens.
        entry.unpack_in(&tmp)?;
    }

    if layer_dir.exists() {
        std::fs::remove_dir_all(layer_dir)?;
    }
    std::fs::rename(&tmp, layer_dir)?;
    Ok(())
}

/// `Read` wrapper that increments a progress bar by the number of bytes
/// each `read` returns. Used to drive the extraction phase of the same
/// per-layer bar that tracked the download.
struct ProgressReader<R: Read> {
    inner: R,
    bar: ProgressBar,
}

impl<R: Read> Read for ProgressReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.bar.inc(n as u64);
        Ok(n)
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use flate2::Compression;
    use flate2::write::GzEncoder;

    use super::*;
    use crate::cache::HOME_LOCK;

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

    /// Build a plain (uncompressed) tar — mirrors what `docker image save`
    /// emits with the classic driver.
    fn build_plain_tarball(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            for (path, content) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                b.append_data(&mut header, path, *content).unwrap();
            }
            b.finish().unwrap();
        }
        buf
    }

    fn fetch_from(src: PathBuf) -> impl FnOnce(&Path) -> anyhow::Result<()> {
        move |dest| {
            std::fs::copy(&src, dest)?;
            Ok(())
        }
    }

    #[test]
    fn ensure_layer_cached_writes_regular_files() {
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

        let layer = ensure_layer_cached("sha256:deadbeef1", fetch_from(tarball), None)
            .expect("extract should succeed");

        assert_eq!(std::fs::read(layer.join("etc/hello")).unwrap(), b"world");
        assert!(layer.join("bin/sh").exists());
    }

    #[test]
    fn ensure_layer_cached_preserves_whiteout_as_xattr() {
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

        let layer = ensure_layer_cached("sha256:deadbeef2", fetch_from(tarball), None).unwrap();

        let whiteout = layer.join("etc/gone");
        assert!(whiteout.exists(), "whiteout placeholder file must exist");
        assert_eq!(std::fs::metadata(&whiteout).unwrap().len(), 0);
        let val = xattr::get(&whiteout, "user.overlay.whiteout").unwrap();
        assert_eq!(val.as_deref(), Some(b"y" as &[u8]));
        assert!(layer.join("etc/keep").exists());
    }

    #[test]
    fn ensure_layer_cached_marks_opaque_directory() {
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

        let layer = ensure_layer_cached("sha256:deadbeef3", fetch_from(tarball), None).unwrap();

        let opaque_dir = layer.join("opt/app");
        let val = xattr::get(&opaque_dir, "user.overlay.opaque").unwrap();
        assert_eq!(val.as_deref(), Some(b"y" as &[u8]));
        assert!(layer.join("opt/app/new").exists());
    }

    #[test]
    fn ensure_layer_cached_is_idempotent() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let tarball = tmp.join("layer.tar.gz");
        std::fs::write(&tarball, build_tarball(&[("a", b"1")])).unwrap();

        let first =
            ensure_layer_cached("sha256:deadbeef4", fetch_from(tarball.clone()), None).unwrap();
        let mtime = std::fs::metadata(&first).unwrap().modified().unwrap();

        // Second call: <digest>/ exists, fetch must not be called.
        let second = ensure_layer_cached(
            "sha256:deadbeef4",
            |_| panic!("fetch must not be called when <digest>/ exists"),
            None,
        )
        .unwrap();
        assert_eq!(first, second);
        assert_eq!(
            std::fs::metadata(&first).unwrap().modified().unwrap(),
            mtime
        );
        // Tarball removed after extraction.
        let layer_parent = first.parent().unwrap();
        let name = first.file_name().unwrap();
        let download = layer_parent.join(format!("{}.download", name.to_string_lossy()));
        assert!(
            !download.exists(),
            "tarball should be removed after extract"
        );
    }

    #[test]
    fn ensure_layer_cached_accepts_plain_tar() {
        // `docker image save` with the classic driver emits uncompressed tars;
        // the unified extractor must dispatch on magic bytes, not assume gzip.
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let tarball = tmp.join("layer.tar");
        std::fs::write(&tarball, build_plain_tarball(&[("etc/plain", b"ok")])).unwrap();

        let layer = ensure_layer_cached("sha256:deadbeef7", fetch_from(tarball), None).unwrap();
        assert_eq!(std::fs::read(layer.join("etc/plain")).unwrap(), b"ok");
    }

    #[test]
    fn ensure_layer_cached_resumes_from_staged_download() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let digest = "sha256:deadbeef5";
        // Pre-stage a complete tarball at <digest>.download as if a previous
        // process had downloaded it but crashed before extraction.
        let layers_root = cache::layers_root().unwrap();
        let name = cache::digest_name(digest);
        let download = layers_root.join(format!("{name}.download"));
        std::fs::write(&download, build_tarball(&[("staged", b"yes")])).unwrap();

        let layer = ensure_layer_cached(
            digest,
            |_| panic!("fetch must not be called when .download exists"),
            None,
        )
        .unwrap();

        assert!(layer.join("staged").exists());
        assert!(!download.exists(), "staged tarball removed after extract");
    }

    #[test]
    fn ensure_layer_cached_cleans_stale_download_tmp() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let digest = "sha256:deadbeef6";
        let layers_root = cache::layers_root().unwrap();
        let name = cache::digest_name(digest);
        let stale = layers_root.join(format!("{name}.download.tmp"));
        std::fs::write(&stale, b"partial garbage").unwrap();

        let tarball_src = tmp.join("layer.tar.gz");
        std::fs::write(&tarball_src, build_tarball(&[("ok", b"yes")])).unwrap();

        let layer = ensure_layer_cached(digest, fetch_from(tarball_src), None).unwrap();
        assert!(layer.join("ok").exists());
        assert!(!stale.exists());
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
