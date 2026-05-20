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
use std::ffi::{CString, OsString};
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Path, PathBuf};

use notify::{RecommendedWatcher, RecursiveMode, Watcher};

/// A sync destination, anchored to its parent directory by an FD opened
/// at sandbox startup. All subsequent writes happen via `*at()` syscalls
/// relative to that FD instead of resolving the source path again at
/// every event — so a post-startup symlink swap of any directory
/// component leading up to the source can't redirect the write.
struct SyncDest {
    /// Original parent directory, pinned by FD. Operations through this
    /// FD target the original inode regardless of what the path may have
    /// been replaced with on disk in the meantime.
    parent_fd: OwnedFd,
    /// Final path component (file name). Stored as `CString` so it's
    /// ready to hand to libc's `*at()` syscalls without per-call
    /// allocation.
    basename: CString,
    /// Original full path. Logging only — never passed to a syscall.
    display: PathBuf,
}

impl SyncDest {
    fn open(source: &Path) -> io::Result<Self> {
        let parent = source.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "source has no parent dir")
        })?;
        let basename_os: OsString = source
            .file_name()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "source has no file name"))?
            .to_os_string();
        let basename = CString::new(basename_os.as_bytes())?;
        let parent_fd: OwnedFd = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(parent)?
            .into();
        Ok(Self {
            parent_fd,
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
/// - The **destination** is anchored to its parent FD captured at
///   sandbox startup (see [`SyncDest`]); every write happens via an
///   `*at()` syscall relative to that FD, so a host-side directory
///   swap of any path component leading up to the source can't
///   redirect the write either.
///
/// Steps:
///
/// 1. `fstat` the overlay FD; reject non-regular entries (a symlink
///    would have failed the `O_NOFOLLOW` open with `ELOOP` already).
/// 2. `fstatat(parent_fd, basename, NOFOLLOW)` — same inode as the
///    overlay → hard link is intact, nothing to do.
/// 3. Re-establish the hard link atomically by calling `linkat` from
///    the overlay FD into `parent_fd/<tmp>` (Linux), then
///    `renameat(parent_fd, tmp, parent_fd, basename)` so future direct
///    writes flow back without needing another sync event.
/// 4. Fall back to an FD-based copy (cross-device, non-Linux, or
///    linkat refused the operation).
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

    // Hard-link check via fstatat against the pinned parent — won't
    // follow a symlink that was just planted at basename either.
    if fstatat_ino_at(&dest.parent_fd, &dest.basename) == Some(overlay_meta.ino()) {
        return;
    }

    let tmp = CString::new(format!(
        ".{}.airlock_sync",
        dest.display
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default()
    ))
    .expect("tmp name has no NULs");

    if linkat_from_fd_at(&overlay_file, &dest.parent_fd, &tmp).is_ok() {
        if renameat_at(&dest.parent_fd, &tmp, &dest.basename).is_ok() {
            tracing::debug!(
                "file sync (hard-link): {} → {}",
                overlay_path.display(),
                dest.display.display()
            );
            return;
        }
        let _ = unlinkat_at(&dest.parent_fd, &tmp);
    }

    match copy_fd_via_parent(&overlay_file, &dest.parent_fd, &tmp, &dest.basename) {
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

/// `fstatat(parent_fd, basename, AT_SYMLINK_NOFOLLOW)` → inode number,
/// or `None` if the entry doesn't exist or is a symlink we refuse to
/// chase. Used only for the "hard link already intact" fast path; any
/// error path falls through to the linkat / copy attempt.
fn fstatat_ino_at(parent_fd: &OwnedFd, basename: &CString) -> Option<u64> {
    let mut st: libc::stat = unsafe { std::mem::zeroed() };
    let rc = unsafe {
        libc::fstatat(
            parent_fd.as_raw_fd(),
            basename.as_ptr(),
            &raw mut st,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if rc != 0 {
        return None;
    }
    Some(st.st_ino as u64)
}

/// Create a new hard link inside `parent_fd` (at `tmp_basename`) pointing
/// at the same inode as the open file behind `src_fd`. On Linux this is
/// `linkat(AT_FDCWD, "/proc/self/fd/N", parent_fd, tmp, AT_SYMLINK_FOLLOW)`
/// — the proc magic link resolves to the FD's underlying inode so the
/// operation targets exactly what we fstat'd. The destination side is
/// anchored to the parent FD, so no path-component lookup happens for
/// the new link either.
#[cfg(target_os = "linux")]
fn linkat_from_fd_at(src_fd: &File, parent_fd: &OwnedFd, tmp_basename: &CString) -> io::Result<()> {
    let src = CString::new(format!("/proc/self/fd/{}", src_fd.as_raw_fd()))?;
    let rc = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            src.as_ptr(),
            parent_fd.as_raw_fd(),
            tmp_basename.as_ptr(),
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
fn linkat_from_fd_at(
    _src_fd: &File,
    _parent_fd: &OwnedFd,
    _tmp_basename: &CString,
) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "linkat from fd not available on this platform",
    ))
}

/// `renameat(parent_fd, from, parent_fd, to)` — atomic rename within
/// the pinned parent directory.
fn renameat_at(parent_fd: &OwnedFd, from: &CString, to: &CString) -> io::Result<()> {
    let rc = unsafe {
        libc::renameat(
            parent_fd.as_raw_fd(),
            from.as_ptr(),
            parent_fd.as_raw_fd(),
            to.as_ptr(),
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `unlinkat(parent_fd, basename, 0)` — used to clean up a leftover
/// temp file when the rename step fails.
fn unlinkat_at(parent_fd: &OwnedFd, basename: &CString) -> io::Result<()> {
    let rc = unsafe { libc::unlinkat(parent_fd.as_raw_fd(), basename.as_ptr(), 0) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Copy the current contents of the open file behind `src_fd` into the
/// pinned parent directory, staged through `tmp` (created with
/// `O_CREAT|O_EXCL` + mode `0600` via `openat`) and atomically renamed
/// to `dest_basename`. Reads come from the FD (not the original path),
/// and every write target is anchored at `parent_fd`, so neither side
/// can be redirected by a post-startup swap.
fn copy_fd_via_parent(
    src_fd: &File,
    parent_fd: &OwnedFd,
    tmp: &CString,
    dest_basename: &CString,
) -> io::Result<()> {
    let raw = unsafe {
        libc::openat(
            parent_fd.as_raw_fd(),
            tmp.as_ptr(),
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            0o600,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh, owned fd.
    let owned = unsafe { OwnedFd::from_raw_fd(raw) };
    let mut out = File::from(owned);

    let mut src = src_fd;
    let copy_result = io::copy(&mut src, &mut out).map(|_| ());
    drop(out);
    let rename_result = copy_result.and_then(|()| renameat_at(parent_fd, tmp, dest_basename));
    if rename_result.is_err() {
        let _ = unlinkat_at(parent_fd, tmp);
    }
    rename_result
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{SyncDest, sync_file};

    fn unique_tmp_dir() -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("airlock-file-sync-test-{n}"));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// A directory in the host source path gets swapped with a symlink
    /// after the sandbox starts. Sync must keep writing into the
    /// originally pinned parent and never follow the symlink to an
    /// attacker-chosen location.
    #[test]
    fn destination_parent_symlink_swap_must_not_redirect_write() {
        let root = unique_tmp_dir();
        let project = root.join("project");
        let safe = project.join("safe");
        let safe_real = project.join("safe.real");
        let outside = root.join("outside");
        let overlay = root.join("overlay");
        fs::create_dir_all(&safe).unwrap();
        fs::create_dir_all(&outside).unwrap();
        fs::create_dir_all(&overlay).unwrap();

        // Destination chosen at startup: parent dir is pinned by FD
        // *before* anything else has a chance to swap it.
        let source = safe.join(".bashrc");
        fs::write(&source, b"ORIGINAL\n").unwrap();
        let dest = SyncDest::open(&source).expect("open dest");

        // New content coming from overlay.
        let overlay_path = overlay.join("mount_key");
        fs::write(&overlay_path, b"PAYLOAD\n").unwrap();

        // Replace source parent dir with a symlink after startup.
        fs::rename(&safe, &safe_real).unwrap();
        std::os::unix::fs::symlink(&outside, &safe).unwrap();

        // Run sync.
        sync_file(&overlay_path, &dest);

        // outside/.bashrc must not be created — the symlink swap must
        // not redirect the write.
        let redirected = outside.join(".bashrc");
        assert!(
            !redirected.exists(),
            "sync followed the planted symlink and wrote outside the pinned parent: {}",
            redirected.display()
        );

        // The pinned parent (now reachable at safe.real/) must have
        // received the new payload — sync should keep working against
        // the location captured at startup, just not the swapped one.
        let original = safe_real.join(".bashrc");
        assert_eq!(
            fs::read_to_string(&original).unwrap(),
            "PAYLOAD\n",
            "sync didn't write to the originally pinned parent"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
