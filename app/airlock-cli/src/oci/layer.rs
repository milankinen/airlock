//! OCI image layer cache.
//!
//! Downloads image layers and extracts each layer into the shared cache. The
//! guest can then stack the cached layers directly to make the container
//! root file system.

use std::io::{self, BufReader, Read};
use std::path::{Component, Path};

use flate2::read::GzDecoder;
use indicatif::ProgressBar;

use crate::util::verified_path::VerifiedPath;
use crate::util::{is_dir, vfs};
use crate::{cache, messages};

/// OCI whiteout marker prefix (an AUFS convention that OCI also uses).
const WHITEOUT_PREFIX: &str = ".wh.";
/// Opaque-directory whiteout filename. It hides all entries at the same path
/// in lower layers.
const OPAQUE_WHITEOUT: &str = ".wh..wh..opq";

/// Monotonic counter that makes staging temp names unique *in* a process.
/// Thus two threads that extract different layers never use the same name.
/// Together with `std::process::id()`, it also keeps separate `airlock`
/// processes out of the staging dirs of the others.
static TMP_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Get a process-unique temp path for the download of a layer tarball.
/// Args:
///  - `layers_root`: Root directory of the layer cache
///  - `key`: Versioned layer key
///
/// Returns:
///   Path `<key>.download.<pid>.<seq>.tmp` in `layers_root`, or error if
///   the check of the path fails.
pub(super) fn download_tmp_path(layers_root: &VerifiedPath, key: &str) -> io::Result<VerifiedPath> {
    // A unique name for each download. Otherwise two `airlock` processes
    // that pull the same uncached image both write one shared file and
    // corrupt the tarball. The rename to the shared `<key>.download` name
    // is the commit.
    layers_root.join(format!(
        "{key}.download.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))
}

/// Make sure that a layer is extracted into the shared cache. Download the
/// tarball with `fetch` only if it is not already on disk.
///
/// A layer goes through these on-disk states under
/// `<data>/oci/layers/`:
///
/// ```text
/// <key>.download.<pid>.<seq>.tmp   # download in progress
/// <key>.download                   # complete tarball, extraction not done
/// <key>.<pid>.<seq>.tmp/           # extraction in progress
/// <key>/                           # finished layer tree (rename = commit)
/// ```
///
/// `<key>` is the versioned layer key of `digest`.
///
/// Each change of state is an atomic rename. After a crash at any point,
/// the next run can remove the leftovers ([`super::gc::sweep`]) or continue
/// from them (this function). The tarball is removed after a successful
/// extraction.
/// Args:
///  - `data_dir`: Airlock data directory
///  - `digest`: Layer digest (`sha256:<hex>`)
///  - `fetch`: Writes the tarball to the given new file path. Called only
///    if no `<key>.download` exists
///  - `progress`: Optional bar to show the extraction progress. Its length,
///    position and message are reset before the extraction.
///
/// Returns:
///   Path of the extracted layer directory.
pub fn ensure_layer_cached<F>(
    data_dir: &VerifiedPath,
    digest: &str,
    fetch: F,
    progress: Option<&ProgressBar>,
) -> anyhow::Result<VerifiedPath>
where
    F: FnOnce(&VerifiedPath) -> anyhow::Result<()>,
{
    let key = cache::layer_key(digest);
    let layer_dir = cache::layer_dir(data_dir, &key)?;
    // Fast path. The directory appears only with the atomic rename at the
    // end of the extraction, so if it exists, the layer is complete.
    if is_dir(&layer_dir) {
        return Ok(layer_dir);
    }

    let parent = cache::layers_root(data_dir)?;
    let download = parent.join(format!("{key}.download"))?;

    // If `<key>.download` exists (from a previous run, or from a caller
    // that staged it, like the docker path), skip `fetch` and extract.
    if vfs::symlink_metadata(&download).is_err() {
        // Also remove an old fixed-name tmp file that an older binary left.
        let _ = vfs::remove_file(&parent.join(format!("{key}.download.tmp"))?);
        let download_tmp = download_tmp_path(&parent, &key)?;
        fetch(&download_tmp)?;
        vfs::rename(&download_tmp, &download)?;
    }

    extract_tarball_to_cache(&parent, &key, &download, progress)?;
    let _ = vfs::remove_file(&download);
    if let Some(pb) = progress {
        pb.set_message(messages::oci::PROGRESS_READY);
    }
    Ok(layer_dir)
}

/// Extract a tarball into a layer directory.
///
/// Extracts into a staging dir, then renames it atomically to `layer_dir`.
/// The rename is the commit point, so readers see `layer_dir` only after a
/// complete extraction. Whiteouts become overlayfs xattrs:
///  * `.wh.<name>` becomes an empty regular file at `<name>` with a
///    `user.overlay.whiteout="y"` xattr. The parent directory gets
///    `user.overlay.opaque="x"`.
///  * `.wh..wh..opq` sets `user.overlay.opaque="y"` on the parent directory.
///    This makes the directory fully opaque (lower layers are hidden there).
///
/// Args:
///  - `parent`: Root directory of the layer cache
///  - `dir_name`: Name of the final layer directory in `parent`
///  - `tarball`: Layer tarball (gzip or plain tar)
///  - `progress`: Optional bar to show the extraction progress.
fn extract_tarball_to_cache(
    parent: &VerifiedPath,
    dir_name: &str,
    tarball: &VerifiedPath,
    progress: Option<&ProgressBar>,
) -> anyhow::Result<()> {
    let layer_dir = parent.join(dir_name)?;
    // Use a process-unique staging dir. Thus two `airlock` processes that
    // extract the same uncached layer never write into the same `.tmp` tree.
    // The final rename below is the commit point for all processes.
    let tmp = parent.join(format!(
        "{dir_name}.{}.{}.tmp",
        std::process::id(),
        TMP_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    ))?;
    let _ = vfs::remove_dir_all(&tmp);
    vfs::create_dir_all(&tmp)?;

    // Layer blobs can be gzip-compressed (OCI spec, registry pulls) or plain
    // tar (`docker image save` with the classic driver). Use the magic bytes
    // to select the decoder.
    let file = vfs::open(tarball)?;
    let file: Box<dyn Read> = match progress {
        Some(pb) => {
            let total = file.metadata().map_or(0, |m| m.len());
            pb.set_length(total);
            pb.set_position(0);
            pb.set_message(messages::oci::PROGRESS_EXTRACTING);
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
                vfs::create_dir_all(&dir)?;
                xattr::set(dir.to_path()?, "user.overlay.opaque", b"y").map_err(|e| {
                    anyhow::anyhow!(messages::oci::set_xattr_failed(
                        "user.overlay.opaque",
                        &dir,
                        e
                    ))
                })?;
            } else {
                // `.wh.`, `.wh..` and `.wh...` name the parent directory or
                // its parent. A delete of that target empties the layer
                // cache, so refuse the layer.
                if matches!(target_name, "" | "." | "..") || target_name.contains('/') {
                    anyhow::bail!(messages::oci::unsafe_whiteout_name(path.display()));
                }
                let dir = safe_join(&tmp, parent_rel)?;
                vfs::create_dir_all(&dir)?;
                let target = dir.join(target_name)?;
                let _ = vfs::remove_file(&target);
                let _ = vfs::remove_dir_all(&target);
                vfs::create_new(&target)?;
                xattr::set(target.to_path()?, "user.overlay.whiteout", b"y").map_err(|e| {
                    anyhow::anyhow!(messages::oci::set_xattr_failed(
                        "user.overlay.whiteout",
                        &target,
                        e
                    ))
                })?;
                // Mark the parent as a directory with xattr whiteouts.
                // Overlayfs readdir examines entries for xattr whiteouts
                // only if the parent has `user.overlay.opaque="x"`. Without
                // it, lookup still returns ENOENT, but directory listings
                // show the "deleted" name as an empty file. Value "y" makes
                // the full dir opaque. Do not overwrite a "y".
                let dir_path = dir.to_path()?;
                let opq = xattr::get(dir_path, "user.overlay.opaque").ok().flatten();
                if opq.as_deref() != Some(b"y") {
                    xattr::set(dir_path, "user.overlay.opaque", b"x").map_err(|e| {
                        anyhow::anyhow!(messages::oci::set_xattr_failed(
                            "user.overlay.opaque=x",
                            &dir,
                            e
                        ))
                    })?;
                }
            }
            continue;
        }

        // `unpack_in` resolves the entry path relative to the extraction
        // root. It also keeps hardlink targets in the root, so a hardlink
        // to an absolute host path cannot occur.
        entry.unpack_in(tmp.to_path()?)?;
    }

    // Commit with an atomic rename. If `<key>/` exists now, a concurrent
    // `airlock` published the same layer first. Use its tree and delete
    // this staging dir. Do not delete the other tree, because the other
    // process can still read it. The other process can also publish between
    // this check and the rename. The rename then fails with ENOTEMPTY.
    // Handle that case the same way.
    if is_dir(&layer_dir) {
        let _ = vfs::remove_dir_all(&tmp);
        return Ok(());
    }
    match vfs::rename(&tmp, &layer_dir) {
        Ok(()) => Ok(()),
        Err(_) if is_dir(&layer_dir) => {
            let _ = vfs::remove_dir_all(&tmp);
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// Join a whiteout path to the extraction root. Refuse a path that can go
/// outside the root.
///
/// Whiteout entries do not go through `entry.unpack_in`, which keeps normal
/// entries in the root. Thus this function does the containment check. A
/// malicious layer can put a symlink in an earlier entry
/// (`etc -> ../../../../home/user`), or use an absolute whiteout path
/// (`/etc/.wh.passwd`). A simple `root.join(parent_rel)` then follows the
/// symlink, or ignores `root` for an absolute `parent_rel`. A whiteout then
/// creates or deletes any host file.
/// Args:
///  - `root`: Extraction root
///  - `rel`: Parent path of the whiteout entry, relative to `root`.
///
/// Returns:
///   The joined path, or error if the path is not safe.
fn safe_join(root: &VerifiedPath, rel: &Path) -> anyhow::Result<VerifiedPath> {
    // Make the path again one component at a time from `root`. Accept only
    // `Normal` components (reject absolute prefixes and any `..`). Refuse
    // if an existing component on the path is a symlink. The extraction
    // loop has one thread, and only this function and `unpack_in` write
    // under `root`. Thus a symlink component can only come from an earlier
    // entry in the same layer.
    let mut cur = root.clone();
    for comp in rel.components() {
        match comp {
            Component::Normal(c) => {
                cur = cur.join(c)?;
                if let Ok(md) = vfs::symlink_metadata(&cur)
                    && md.file_type().is_symlink()
                {
                    anyhow::bail!(messages::oci::whiteout_traverses_symlink(&cur));
                }
            }
            Component::CurDir => {}
            _ => anyhow::bail!(messages::oci::unsafe_whiteout_component(rel.display())),
        }
    }
    Ok(cur)
}

/// `Read` wrapper that increments a progress bar by the number of bytes
/// that each `read` returns. Shows the extraction progress on the same
/// per-layer bar that showed the download.
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
