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
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
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
            sync_file(&overlay_path, source);
        }
    }

    Ok(())
}

/// Sync `overlay_path` back to `source` using the cheapest available method.
///
/// The overlay file is writable by the guest over virtiofs, so between
/// successive sync events the guest can legitimately (atomic rename) or
/// maliciously (symlink swap) replace the directory entry. To avoid a
/// check-then-use TOCTOU the overlay is opened **once** with `O_NOFOLLOW`
/// at the top, and every subsequent operation targets the resulting FD:
///
/// 1. `fstat` the FD; reject non-regular entries (a symlink would have
///    failed the `O_NOFOLLOW` open with `ELOOP` already).
/// 2. Same inode as `source` → hard link is intact, nothing to do.
/// 3. Re-establish the hard link atomically by calling `linkat` on the
///    FD via `/proc/self/fd/<n>` (Linux), then `rename` into place so
///    future direct writes flow back without needing another sync event.
/// 4. Fall back to an FD-based copy (cross-device, non-Linux, or linkat
///    refused the operation).
fn sync_file(overlay_path: &Path, source: &Path) {
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

    if std::fs::metadata(source).is_ok_and(|m| m.ino() == overlay_meta.ino()) {
        return;
    }

    let tmp = source.with_file_name(format!(
        ".{}.airlock_sync",
        source.file_name().unwrap_or_default().to_string_lossy()
    ));

    if linkat_from_fd(&overlay_file, &tmp).is_ok() {
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

    match copy_fd_to_path(&overlay_file, &tmp, source) {
        Ok(()) => tracing::debug!(
            "file sync (copy): {} → {}",
            overlay_path.display(),
            source.display()
        ),
        Err(e) => tracing::warn!("file sync {}: {e}", source.display()),
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

/// Create a new hard link at `dest` pointing at the same inode as the open
/// file behind `fd`, without ever using a path that a concurrent attacker
/// could swap. On Linux this is `linkat(AT_FDCWD, "/proc/self/fd/N", ..., AT_SYMLINK_FOLLOW)`
/// — the proc magic link resolves to the FD's underlying inode so the
/// operation targets exactly what we fstat'd, regardless of what lives at
/// `overlay_path` by the time the syscall runs.
#[cfg(target_os = "linux")]
fn linkat_from_fd(fd: &File, dest: &Path) -> io::Result<()> {
    let src = std::ffi::CString::new(format!("/proc/self/fd/{}", fd.as_raw_fd()))?;
    let dst = std::ffi::CString::new(dest.as_os_str().as_bytes())?;
    let rc = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            src.as_ptr(),
            libc::AT_FDCWD,
            dst.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// macOS lacks `/proc/self/fd` and `linkat(AT_EMPTY_PATH)`, so there's no
/// portable way to create a hard link from an already-open FD. Signal
/// "unsupported" so `sync_file` falls through to the FD-based copy path.
#[cfg(not(target_os = "linux"))]
fn linkat_from_fd(_fd: &File, _dest: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "linkat from fd not available on this platform",
    ))
}

/// Copy the current contents of the open file behind `fd` to `dest`,
/// staged through `tmp` and atomically renamed at the end. Reads come
/// from the FD (not the original path) so a post-open symlink swap can't
/// redirect the source of the copy. `tmp` is created with `O_CREAT|O_EXCL`
/// + mode `0600` so it can't clobber an existing entry.
fn copy_fd_to_path(fd: &File, tmp: &Path, dest: &Path) -> io::Result<()> {
    let mut src = fd;
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(tmp)?;
    let copy_result = io::copy(&mut src, &mut out).map(|_| ());
    let rename_result = copy_result.and_then(|()| std::fs::rename(tmp, dest));
    if rename_result.is_err() {
        let _ = std::fs::remove_file(tmp);
    }
    rename_result
}
