//! Sync of writable file mounts.
//!
//! Copies the changes that the guest makes in writable file mounts back to
//! their source files on the host. The sync runs in the background until the
//! session stops it.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::util::PinnedDir;

/// A sync destination, held by an FD of its parent directory that was opened
/// at sandbox start.
///
/// All later writes use `*at()` syscalls relative to that FD. The source
/// path is not resolved again on each event. Thus, after the start, a
/// symlink swap of a directory on the path to the source cannot redirect
/// the write.
struct SyncDest {
    /// Original parent directory, pinned by FD. Operations through it use
    /// the original inode, also if something replaced the path on disk.
    parent: PinnedDir,
    /// Last path component (file name).
    basename: OsString,
    /// Original full path. Only for logs, never given to a syscall.
    display: PathBuf,
}

impl SyncDest {
    /// Pin the parent directory of `source` and keep its file name.
    fn open(source: &Path) -> io::Result<Self> {
        let parent = source.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "source has no parent dir")
        })?;
        let basename = source
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no file name"))?
            .to_os_string();
        Ok(Self {
            parent: PinnedDir::pin(parent)?,
            basename,
            display: source.to_path_buf(),
        })
    }
}

/// Handle to the running file-sync task. A drop aborts the task
/// immediately. Call `shutdown()` to handle pending events first.
pub(super) struct SyncHandle {
    task: Option<tokio::task::JoinHandle<()>>,
    /// A drop of the watcher closes the event channel. The task then
    /// handles the buffered events and stops normally.
    watcher: Option<RecommendedWatcher>,
}

impl SyncHandle {
    /// Stop the sync task cleanly. Drop the watcher (no new events), then
    /// wait until the task handles the remaining events and stops.
    pub(super) async fn shutdown(mut self) {
        drop(self.watcher.take());
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        // Fallback for error paths that did not call `shutdown()`.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Start a background task that watches the overlay files of writable file
/// mounts and syncs changes back to their source paths on the host.
///
/// The task watches `overlay/files/rw/` with the native file-change API of
/// the OS (FSEvents on macOS, inotify on Linux). File mounts are hardlinks
/// into this directory. If the guest writes atomically (temp file and
/// rename), the VirtioFS server replaces the directory entry with a new
/// inode. That breaks the link to the source file. The task finds such
/// changes and links the file again (or copies it), so the host source file
/// stays current.
/// Args:
///  - `mounts`: All resolved mounts. Only writable file mounts are synced
///  - `overlay_dir`: Overlay directory of the sandbox.
///
/// Returns:
///   The task handle, or `None` if there are no writable file mounts or the
///   watcher setup fails.
pub(super) fn start(
    mounts: &[super::mount::ResolvedMount],
    overlay_dir: &Path,
) -> Option<SyncHandle> {
    let files_rw_dir = overlay_dir.join("files").join("rw");
    // Open the parent dir of each writable mount now. If a parent cannot be
    // opened (missing, not readable, not a dir), log a warning and never
    // sync that mount. That is better than a silent write to whatever is
    // at that path later.
    let rw_files: Vec<(String, SyncDest)> = mounts
        .iter()
        .filter(|m| matches!(m.mount_type, super::mount::MountType::File { .. }) && !m.read_only)
        .filter_map(|m| match SyncDest::open(&m.source) {
            Ok(dest) => Some((m.key().to_string(), dest)),
            Err(e) => {
                tracing::warn!(
                    "file sync skipping {} (parent dir not pinnable): {e}",
                    m.source.display()
                );
                None
            }
        })
        .collect();

    if rw_files.is_empty() {
        return None;
    }

    let (tx, rx) = tokio::sync::mpsc::channel::<notify::Result<notify::Event>>(32);
    let mut watcher = match RecommendedWatcher::new(
        move |res| {
            let _ = tx.try_send(res);
        },
        notify::Config::default(),
    ) {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!("file sync watcher init failed: {e}");
            return None;
        }
    };
    if let Err(e) = watcher.watch(&files_rw_dir, RecursiveMode::NonRecursive) {
        tracing::warn!("file sync watch failed: {e}");
        return None;
    }

    let task = tokio::task::spawn_local(async move {
        if let Err(e) = watch_loop(files_rw_dir, rw_files, rx).await {
            tracing::warn!("file sync loop error: {e}");
        }
    });

    Some(SyncHandle {
        task: Some(task),
        watcher: Some(watcher),
    })
}

/// Handle watcher events until the watcher is dropped, and sync each
/// changed file to its destination.
async fn watch_loop(
    files_rw_dir: PathBuf,
    rw_files: Vec<(String, SyncDest)>,
    mut rx: tokio::sync::mpsc::Receiver<notify::Result<notify::Event>>,
) -> anyhow::Result<()> {
    // (ino, mtime_sec, mtime_nsec). Detects direct writes (mtime changes)
    // and atomic renames (new inode).
    type FileState = (u64, i64, i64);

    let read_state = |key: &str| -> Option<FileState> {
        let m = std::fs::metadata(files_rw_dir.join(key)).ok()?;
        Some((m.ino(), m.mtime(), m.mtime_nsec()))
    };

    let file_map: HashMap<String, SyncDest> = rw_files.into_iter().collect();

    // Record the initial state, so the first event does not cause a false sync.
    let mut states: HashMap<String, FileState> = file_map
        .keys()
        .filter_map(|key| read_state(key).map(|s| (key.clone(), s)))
        .collect();

    // The loop stops when the watcher is dropped (tx closes, recv gives None).
    while let Some(res) = rx.recv().await {
        let event = match res {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!("file sync event error: {e}");
                continue;
            }
        };

        for path in &event.paths {
            let Some(filename) = path.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            let Some(dest) = file_map.get(filename) else {
                continue;
            };
            let Some(new_state) = read_state(filename) else {
                continue;
            };
            let old_state = states.get(filename).copied();
            if old_state == Some(new_state) {
                continue;
            }
            states.insert(filename.to_string(), new_state);
            // The first state is the boot-time baseline. Do not sync yet.
            let Some(_) = old_state else { continue };

            let overlay_path = files_rw_dir.join(filename);
            sync_file(&overlay_path, dest);
        }
    }

    Ok(())
}

/// Sync an overlay file back to its destination with the cheapest
/// available method.
///
/// Both sides of the sync are pinned:
///  * The **overlay** is opened **one time** with `O_NOFOLLOW`, and all
///    later reads use that FD. Thus a guest-side symlink swap of the overlay
///    entry between events cannot redirect the read.
///  * The **destination** is pinned to its parent directory from sandbox
///    start (see [`SyncDest`] and [`PinnedDir`]). Each write uses an `*at()`
///    syscall relative to it. Thus a host-side directory swap on the path to
///    the source cannot redirect the write.
fn sync_file(overlay_path: &Path, dest: &SyncDest) {
    // Steps:
    //  1. `fstat` the overlay FD. Reject entries that are not regular files
    //     (a symlink already failed the `O_NOFOLLOW` open with `ELOOP`).
    //  2. Same inode at the destination (no-follow): the hardlink is
    //     intact, nothing to do.
    //  3. Make the hardlink again atomically (`PinnedDir::link_from`,
    //     Linux), so later direct writes go back without a new sync event.
    //  4. Use an FD-based copy (`PinnedDir::copy_from`) if a link is not
    //     possible (cross-device, not Linux).
    let overlay_file = match open_nofollow(overlay_path) {
        Ok(f) => f,
        Err(e) => {
            // ELOOP here means that the guest put a symlink here. Skip it
            // with a warning.
            tracing::warn!("file sync open {}: {e}", overlay_path.display());
            return;
        }
    };
    let overlay_meta = match overlay_file.metadata() {
        Ok(m) => m,
        Err(e) => {
            tracing::warn!("file sync fstat {}: {e}", overlay_path.display());
            return;
        }
    };
    if !overlay_meta.file_type().is_file() {
        tracing::warn!(
            "file sync skipped non-regular overlay entry {}",
            overlay_path.display()
        );
        return;
    }

    // Hardlink check against the pinned parent. It also does not follow a
    // symlink that something just put at basename.
    if dest.parent.ino(&dest.basename) == Some(overlay_meta.ino()) {
        return;
    }

    if dest.parent.link_from(&overlay_file, &dest.basename).is_ok() {
        tracing::debug!(
            "file sync (hard-link): {} → {}",
            overlay_path.display(),
            dest.display.display()
        );
        return;
    }

    match dest.parent.copy_from(&overlay_file, &dest.basename, 0o600) {
        Ok(()) => tracing::debug!(
            "file sync (copy): {} → {}",
            overlay_path.display(),
            dest.display.display()
        ),
        Err(e) => tracing::warn!("file sync {}: {e}", dest.display.display()),
    }
}

/// Open `path` with `O_NOFOLLOW`. If `path` is a symbolic link, the open
/// fails with `ELOOP` and does not silently redirect the read. Later
/// operations use the returned FD, never the path.
fn open_nofollow(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(test)]
mod tests {
    //! Tests for the symlink safety of the file mount sync.

    use std::fs;

    use super::{SyncDest, sync_file};
    use crate::test_cfg::temp_dir;

    /// Test that a sync writes to the parent directory that was pinned at the
    /// start, also when the parent path changes to a symlink later. Thus a
    /// symlink swap cannot redirect the write to another host place.
    ///   1. Pin the destination of a mounted file in the project
    ///   2. Rename the parent directory and put a symlink to an outside
    ///      directory at its old path
    ///   3. Sync the guest copy of the file
    ///   4. Check that nothing went to the outside directory and the renamed
    ///      parent got the new content
    #[test]
    fn sync_after_destination_parent_swapped_for_symlink_writes_to_pinned_parent() {
        let tmp = temp_dir();
        let root = tmp.path();
        let safe = root.join("project/safe");
        let safe_real = root.join("project/safe.real");
        let outside = root.join("outside");
        let overlay = root.join("overlay");
        fs::create_dir_all(&safe).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&overlay).unwrap();
        let source = safe.join(".bashrc");
        fs::write(&source, b"ORIGINAL\n").unwrap();
        let dest = SyncDest::open(&source).unwrap();
        let overlay_path = overlay.join("mount_key");
        fs::write(&overlay_path, b"PAYLOAD\n").unwrap();

        fs::rename(&safe, &safe_real).unwrap();
        std::os::unix::fs::symlink(&outside, &safe).unwrap();
        sync_file(&overlay_path, &dest);

        assert!(!outside.join(".bashrc").exists());
        assert_eq!(
            fs::read_to_string(safe_real.join(".bashrc")).unwrap(),
            "PAYLOAD\n"
        );
    }
}
