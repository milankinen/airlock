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
use std::path::{Component, Path, PathBuf};

use flate2::read::GzDecoder;
use indicatif::ProgressBar;

use crate::cache;

/// OCI whiteout marker prefix (AUFS convention, inherited by OCI).
const WHITEOUT_PREFIX: &str = ".wh.";
/// Opaque-directory whiteout filename — clears all siblings at the same path
/// in lower layers.
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

/// Monotonic counter that makes staging temp names unique *within* a process,
/// so two threads extracting different layers never collide on a name either.
/// Combined with `std::process::id()` it also keeps separate `airlock`
/// processes off each other's staging dirs.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

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
    let key = cache::layer_key(digest);
    let layer_dir = cache::layer_dir(&key)?;
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

    if !download.exists() {
        // Stage into a process-unique temp file so two `airlock` processes
        // pulling the same uncached image don't both write the one shared
        // `<digest>.download.tmp` and corrupt each other's tarball; the rename
        // to the shared `<digest>.download` name is the commit. A stale
        // fixed-name tmp left by an older binary is cleaned up here too.
        let _ = std::fs::remove_file(parent.join(format!("{dir_name}.download.tmp")));
        let download_tmp = parent.join(format!(
            "{dir_name}.download.{}.{}.tmp",
            std::process::id(),
            TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
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
///   `user.overlay.whiteout="y"` xattr, and the parent directory gets
///   `user.overlay.opaque="x"` (the userspace opt-in marker — without it
///   overlayfs only honors the whiteout on lookup, not during readdir, so
///   the deleted name reappears in directory listings). The "x" marker is
///   distinct from "y": the latter hides lowers entirely.
/// - `.wh..wh..opq` sets `user.overlay.opaque="y"` on the parent directory,
///   marking it as fully opaque (lowers hidden at that directory).
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
    // Stage into a process-unique dir so two `airlock` processes extracting
    // the same uncached layer never share (and clobber) the one `.tmp` tree.
    // The final rename below is the cross-process commit point.
    let tmp = parent.join(format!(
        "{dir_name}.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ));
    let _ = std::fs::remove_dir_all(&tmp);
    std::fs::create_dir_all(&tmp)?;

    // Layer blobs may be gzip-compressed (OCI spec, registry pulls) or plain
    // tar (`docker image save` with the classic driver) — dispatch on magic.
    let file = std::fs::File::open(tarball)?;
    let file: Box<dyn Read> = match progress {
        Some(pb) => {
            let total = file.metadata().map_or(0, |m| m.len());
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
                let dir = safe_join(&tmp, parent_rel)?;
                std::fs::create_dir_all(&dir)?;
                xattr::set(&dir, "user.overlay.opaque", b"y").map_err(|e| {
                    anyhow::anyhow!(
                        "set user.overlay.opaque on {}: {e} \
                         (host filesystem must support user xattrs)",
                        dir.display()
                    )
                })?;
            } else {
                let dir = safe_join(&tmp, parent_rel)?;
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
                // Mark the parent directory as containing xattr whiteouts.
                // overlayfs only scans entries for xattr-based whiteouts when
                // the parent carries `user.overlay.opaque="x"` — without it
                // the lookup path still returns ENOENT (it reads the xattr
                // directly) but the readdir merge-iteration path treats the
                // file as a plain 0-byte regular and the "deleted" name
                // reappears in directory listings. Note: value "x" is the
                // userspace opt-in marker, distinct from "y" which makes the
                // whole dir opaque (lowers hidden). Don't overwrite an
                // existing "y".
                let opq = xattr::get(&dir, "user.overlay.opaque").ok().flatten();
                if opq.as_deref() != Some(b"y") {
                    xattr::set(&dir, "user.overlay.opaque", b"x").map_err(|e| {
                        anyhow::anyhow!(
                            "set user.overlay.opaque=x on {}: {e} \
                             (host filesystem must support user xattrs)",
                            dir.display()
                        )
                    })?;
                }
            }
            continue;
        }

        // `unpack_in` resolves the entry path relative to the extraction root
        // and — critically — rewrites hardlink targets to stay inside it, so
        // `ln /absolute/host/path /extract/root/foo` never happens.
        entry.unpack_in(&tmp)?;
    }

    // Commit via atomic rename. The fast path in `ensure_layer_cached`
    // returned early if `<digest>/` already existed, so its presence here
    // means a concurrent `airlock` won the race and published the same layer:
    // reuse its tree and drop our staging dir rather than deleting a directory
    // a peer may still be reading. The winner can also appear between this
    // check and the rename (which then fails with ENOTEMPTY); treat that the
    // same way.
    if layer_dir.is_dir() {
        let _ = std::fs::remove_dir_all(&tmp);
        return Ok(());
    }
    match std::fs::rename(&tmp, layer_dir) {
        Ok(()) => Ok(()),
        Err(_) if layer_dir.is_dir() => {
            let _ = std::fs::remove_dir_all(&tmp);
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Join `rel` onto the extraction `root` for whiteout handling, refusing any
/// path that could escape the root.
///
/// Whiteout entries are handled with our own filesystem calls (rather than
/// `entry.unpack_in`, which contains normal entries for us), so we must do
/// the containment ourselves. A malicious layer can order an earlier entry
/// that plants a symlink (`etc -> ../../../../home/user`) or use an absolute
/// whiteout path (`/etc/.wh.passwd`); a naive `root.join(parent_rel)` would
/// then follow the symlink or, for an absolute `parent_rel`, discard `root`
/// entirely — turning a whiteout into an arbitrary host-file create/delete.
///
/// We rebuild the path one component at a time from `root`, accept only
/// `Normal` components (rejecting absolute prefixes and any leftover `..`),
/// and refuse if any existing component along the way is a symlink. Because
/// the extraction loop is single-threaded and only this function and
/// `unpack_in` write under `root`, a component that is a symlink can only
/// have come from an earlier entry in the same layer.
fn safe_join(root: &Path, rel: &Path) -> anyhow::Result<PathBuf> {
    let mut cur = root.to_path_buf();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => {
                cur.push(c);
                if let Ok(md) = std::fs::symlink_metadata(&cur)
                    && md.file_type().is_symlink()
                {
                    anyhow::bail!(
                        "refusing layer: whiteout path traverses a symlink at {}",
                        cur.display()
                    );
                }
            }
            Component::CurDir => {}
            _ => anyhow::bail!(
                "refusing layer: unsafe whiteout path component in {}",
                rel.display()
            ),
        }
    }
    Ok(cur)
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
        // Parent dir must carry `user.overlay.opaque="x"` so overlayfs
        // recognizes xattr whiteouts inside it during readdir, not just
        // lookup.
        let parent_opq = xattr::get(layer.join("etc"), "user.overlay.opaque").unwrap();
        assert_eq!(parent_opq.as_deref(), Some(b"x" as &[u8]));
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
        // Layer dir on disk must carry the `{LAYER_FORMAT}.` prefix — if it
        // didn't, pre-migration cached layers (without `user.overlay.opaque=x`
        // on whiteout parents) would silently shadow fresh extracts.
        assert!(
            name.to_string_lossy()
                .starts_with(&format!("{}.", cache::LAYER_FORMAT))
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
        // Pre-stage a complete tarball at <key>.download as if a previous
        // process had downloaded it but crashed before extraction.
        let layers_root = cache::layers_root().unwrap();
        let key = cache::layer_key(digest);
        let download = layers_root.join(format!("{key}.download"));
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
        let key = cache::layer_key(digest);
        let stale = layers_root.join(format!("{key}.download.tmp"));
        std::fs::write(&stale, b"partial garbage").unwrap();

        let tarball_src = tmp.join("layer.tar.gz");
        std::fs::write(&tarball_src, build_tarball(&[("ok", b"yes")])).unwrap();

        let layer = ensure_layer_cached(digest, fetch_from(tarball_src), None).unwrap();
        assert!(layer.join("ok").exists());
        assert!(!stale.exists());
    }

    #[test]
    fn extract_reuses_existing_winner_dir() {
        // Models the concurrent-pull commit race: `<digest>/` already exists
        // (a peer process won), so extraction must reuse it and drop its own
        // staging tree instead of clobbering a dir a peer may be reading.
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let key = cache::layer_key("sha256:winnerdir");
        let layer_dir = cache::layer_dir(&key).unwrap();
        std::fs::create_dir_all(&layer_dir).unwrap();
        std::fs::write(layer_dir.join("winner"), b"kept").unwrap();

        let tarball = tmp.join("layer.tar.gz");
        std::fs::write(&tarball, build_tarball(&[("loser", b"x")])).unwrap();

        extract_tarball_to_cache(&layer_dir, &tarball, None)
            .expect("reuse must succeed when the winner dir already exists");

        // Winner's tree is untouched; our entry was not committed over it.
        assert_eq!(std::fs::read(layer_dir.join("winner")).unwrap(), b"kept");
        assert!(!layer_dir.join("loser").exists());

        // No staging dir left behind.
        let parent = layer_dir.parent().unwrap();
        let stray: Vec<_> = std::fs::read_dir(parent)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".tmp"))
            .collect();
        assert!(stray.is_empty(), "staging tmp dir must be cleaned up");
    }

    #[test]
    fn safe_join_accepts_normal_relative_path() {
        let root = tempfile_dir();
        assert_eq!(
            safe_join(&root, Path::new("a/b/c")).unwrap(),
            root.join("a/b/c")
        );
    }

    #[test]
    fn safe_join_rejects_absolute_path() {
        let root = tempfile_dir();
        // An absolute whiteout path (`/etc/.wh.passwd`) would make a naive
        // `root.join(parent_rel)` discard `root` entirely.
        assert!(safe_join(&root, Path::new("/etc")).is_err());
    }

    #[test]
    fn safe_join_rejects_symlink_component() {
        let root = tempfile_dir();
        // An earlier layer entry plants `esc` as a symlink pointing outside.
        std::os::unix::fs::symlink("/", root.join("esc")).unwrap();
        assert!(safe_join(&root, Path::new("esc")).is_err());
        assert!(safe_join(&root, Path::new("esc/passwd")).is_err());
    }

    #[test]
    fn ensure_layer_cached_wont_delete_outside_root_via_whiteout() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        // A sentinel host file outside the extraction root. A whiteout that
        // follows a planted symlink would `remove_file` it.
        let outside = tmp.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("victim"), b"precious").unwrap();

        // Malicious layer: plant `esc -> <outside>`, then whiteout
        // `esc/.wh.victim` to try to delete `<outside>/victim`.
        let mut gz = GzEncoder::new(Vec::new(), Compression::fast());
        {
            let mut b = tar::Builder::new(&mut gz);
            let mut link = tar::Header::new_gnu();
            link.set_entry_type(tar::EntryType::Symlink);
            link.set_size(0);
            b.append_link(&mut link, "esc", &outside).unwrap();
            let mut wh = tar::Header::new_gnu();
            wh.set_size(0);
            wh.set_mode(0o644);
            wh.set_cksum();
            b.append_data(&mut wh, "esc/.wh.victim", &b""[..]).unwrap();
            b.finish().unwrap();
        }
        let buf = gz.finish().unwrap();
        let tarball = tmp.join("evil.tar.gz");
        std::fs::write(&tarball, buf).unwrap();

        let _ = ensure_layer_cached("sha256:evil1", fetch_from(tarball), None);

        // The security invariant: the file outside the root is untouched,
        // regardless of whether extraction errored or contained the whiteout.
        assert!(
            outside.join("victim").exists(),
            "whiteout escaped the extraction root and deleted a host file"
        );
        assert_eq!(std::fs::read(outside.join("victim")).unwrap(), b"precious");
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
