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
