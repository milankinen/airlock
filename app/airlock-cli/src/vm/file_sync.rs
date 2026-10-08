//! Host-side file-mount sync: watches overlay/files/rw/ with the OS-native
//! file-change API (FSEvents on macOS, inotify on Linux) and syncs changes
//! back to the original source paths on the host.
//!
//! File mounts are backed by hard links into the project overlay directory.
//! When the guest writes atomically (temp file + rename), virtiofsd replaces
//! the directory entry with a new inode, severing the link to the source file.
//! This module detects such changes and re-establishes the link (or falls back
//! to a copy) so the host source file stays up-to-date.

use std::collections::HashMap;
use std::ffi::OsString;
use std::fs::File;
use std::io;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

use crate::util::PinnedDir;

/// A sync destination, anchored to its parent directory by an FD opened
/// at sandbox startup. All subsequent writes happen via `*at()` syscalls
/// relative to that FD instead of resolving the source path again at
/// every event — so a post-startup symlink swap of any directory
/// component leading up to the source can't redirect the write.
struct SyncDest {
    /// Original parent directory, pinned by FD. Operations through it
    /// target the original inode regardless of what the path may have
    /// been replaced with on disk in the meantime.
    parent: PinnedDir,
    /// Final path component (file name).
    basename: OsString,
    /// Original full path. Logging only — never passed to a syscall.
    display: PathBuf,
}

impl SyncDest {
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
/// Handle to the running file-sync task. Dropping aborts immediately;
/// call `shutdown()` to drain pending events first.
pub(super) struct SyncHandle {
    task: Option<tokio::task::JoinHandle<()>>,
    /// Dropping the watcher closes the event channel, which lets the task
    /// drain any buffered events and exit naturally.
    watcher: Option<RecommendedWatcher>,
}

impl SyncHandle {
    /// Gracefully stop the sync task: drop the watcher (stops new events),
    /// then wait for the task to drain remaining events and finish.
    pub(super) async fn shutdown(mut self) {
        drop(self.watcher.take());
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for SyncHandle {
    fn drop(&mut self) {
        // Fallback for error paths where shutdown() wasn't called.
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

/// Spawn a background task that watches rw file-mount overlay files and syncs
/// changes back to their original source paths on the host.
///
/// Returns `None` when there are no rw file mounts or the watcher can't be set up.
pub(super) fn start(
    mounts: &[super::mount::ResolvedMount],
    overlay_dir: &Path,
) -> Option<SyncHandle> {
    let files_rw_dir = overlay_dir.join("files").join("rw");
    // Open each rw mount's parent dir up front. Mounts whose parent
    // can't be opened (gone, unreadable, replaced by a non-dir) are
    // skipped loudly and never sync'd — better than silently writing
    // to whatever happens to live at that path later.
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

async fn watch_loop(
    files_rw_dir: PathBuf,
    rw_files: Vec<(String, SyncDest)>,
    mut rx: tokio::sync::mpsc::Receiver<notify::Result<notify::Event>>,
) -> anyhow::Result<()> {
    // (ino, mtime_sec, mtime_nsec) — catches both direct writes (mtime changes)
    // and atomic renames (new inode).
    type FileState = (u64, i64, i64);

    let read_state = |key: &str| -> Option<FileState> {
        let m = std::fs::metadata(files_rw_dir.join(key)).ok()?;
        Some((m.ino(), m.mtime(), m.mtime_nsec()))
    };

    let file_map: HashMap<String, SyncDest> = rw_files.into_iter().collect();

    // Capture initial state so the first event doesn't trigger a spurious sync.
    let mut states: HashMap<String, FileState> = file_map
        .keys()
        .filter_map(|key| read_state(key).map(|s| (key.clone(), s)))
        .collect();

    // Loop exits naturally when the watcher is dropped (tx closes, recv → None).
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
            // First observation is the boot-time baseline — don't sync yet.
            let Some(_) = old_state else { continue };

            let overlay_path = files_rw_dir.join(filename);
            sync_file(&overlay_path, dest);
        }
    }

    Ok(())
}

/// Sync `overlay_path` back to `dest` using the cheapest available method.
///
/// Both ends of the sync are anchored:
///
/// - The **overlay** is opened **once** with `O_NOFOLLOW` and every
///   subsequent read targets the resulting FD, so a guest-side symlink
///   swap of the overlay entry between events can't redirect the read.
/// - The **destination** is anchored to its parent directory pinned at
///   sandbox startup (see [`SyncDest`] and [`PinnedDir`]); every write
///   happens via an `*at()` syscall relative to it, so a host-side
///   directory swap of any path component leading up to the source
///   can't redirect the write either.
///
/// Steps:
///
/// 1. `fstat` the overlay FD; reject non-regular entries (a symlink
///    would have failed the `O_NOFOLLOW` open with `ELOOP` already).
/// 2. Same inode at the destination (no-follow) → the hard link is
///    intact, nothing to do.
/// 3. Re-establish the hard link atomically ([`PinnedDir::link_from`],
///    Linux) so future direct writes flow back without needing another
///    sync event.
/// 4. Fall back to an FD-based copy ([`PinnedDir::copy_from`]) when
///    linking is not possible (cross-device, non-Linux).
fn sync_file(overlay_path: &Path, dest: &SyncDest) {
    let overlay_file = match open_nofollow(overlay_path) {
        Ok(f) => f,
        Err(e) => {
            // ELOOP here means the guest planted a symlink — skip loudly.
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

    // Hard-link check against the pinned parent — won't follow a symlink
    // that was just planted at basename either.
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

/// Open `path` with `O_NOFOLLOW` so a symbolic link at `path` aborts the
/// open with `ELOOP` rather than silently redirecting the read. Subsequent
/// operations use the returned FD, never the path.
fn open_nofollow(path: &Path) -> io::Result<File> {
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::{SyncDest, sync_file};
    use crate::test_cfg::temp_dir;

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
