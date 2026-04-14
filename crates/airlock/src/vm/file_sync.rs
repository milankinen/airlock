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
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

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
    let rw_files: Vec<(String, PathBuf)> = mounts
        .iter()
        .filter(|m| matches!(m.mount_type, super::mount::MountType::File { .. }) && !m.read_only)
        .map(|m| (m.key().to_string(), m.source.clone()))
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
    rw_files: Vec<(String, PathBuf)>,
    mut rx: tokio::sync::mpsc::Receiver<notify::Result<notify::Event>>,
) -> anyhow::Result<()> {
    // (ino, mtime_sec, mtime_nsec) — catches both direct writes (mtime changes)
    // and atomic renames (new inode).
    type FileState = (u64, i64, i64);

    let read_state = |key: &str| -> Option<FileState> {
        let m = std::fs::metadata(files_rw_dir.join(key)).ok()?;
        Some((m.ino(), m.mtime(), m.mtime_nsec()))
    };

    let file_map: HashMap<String, PathBuf> = rw_files.into_iter().collect();

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
            let Some(source) = file_map.get(filename) else {
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
            sync_file(&overlay_path, source).await;
        }
    }

    Ok(())
}

/// Sync `overlay_path` back to `source` using the cheapest available method:
///
/// 1. Same inode → hard link is intact, source already has the new content.
/// 2. Re-establish the hard link atomically (`hard_link` + `rename`), so future
///    direct writes flow back without needing another sync event.
/// 3. Fall back to `tokio::fs::copy` (e.g. cross-device, permissions).
async fn sync_file(overlay_path: &Path, source: &Path) {
    // Step 1: if both paths share an inode the hard link is still intact —
    // source already has the updated content, nothing to do.
    let overlay_ino = match std::fs::metadata(overlay_path) {
        Ok(m) => m.ino(),
        Err(e) => {
            tracing::warn!("file sync stat {}: {e}", overlay_path.display());
            return;
        }
    };
    if std::fs::metadata(source).is_ok_and(|m| m.ino() == overlay_ino) {
        return;
    }

    // Step 2: atomically re-establish the hard link so future direct writes
    // flow through without another sync event.
    //   hard_link(overlay → tmp)  — create link in source's directory
    //   rename(tmp → source)      — atomically replace source
    let tmp = source.with_file_name(format!(
        ".{}.airlock_sync",
        source.file_name().unwrap_or_default().to_string_lossy()
    ));
    if std::fs::hard_link(overlay_path, &tmp).is_ok() {
        if std::fs::rename(&tmp, source).is_ok() {
            tracing::debug!(
                "file sync (hard-link): {} → {}",
                overlay_path.display(),
                source.display()
            );
            return;
        }
        let _ = std::fs::remove_file(&tmp);
    }

    // Step 3: fall back to an async copy (offloads I/O to a blocking thread).
    match tokio::fs::copy(overlay_path, source).await {
        Ok(_) => tracing::debug!(
            "file sync (copy): {} → {}",
            overlay_path.display(),
            source.display()
        ),
        Err(e) => tracing::warn!("file sync {}: {e}", source.display()),
    }
}
