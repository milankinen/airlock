//! Container rootfs.
//!
//! Makes the container rootfs from the image layers, the project state and the
//! configured mounts. Hides sandbox internals and the masked paths from the
//! container.

use std::ffi::OsStr;
use std::io::Read;
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use tracing::{debug, info, warn};

use super::safe_resolve::{self, Policy};
use crate::init::MountConfig;

/// Assemble the container rootfs at `/mnt/overlay/rootfs`.
///
/// Steps:
///  1. Mount overlayfs. The OCI image layers are the lowerdirs (topmost
///     first). The upperdir is on the project disk, or on tmpfs if there is
///     no disk. Before the mount:
///     - Resets the upperdir if the image changed, so old upperdir paths of
///       a previous image do not hide the new image.
///     - Writes the file-mount symlinks into the upperdir.
///     - Adds the CA bundles as a tmpfs lowerdir (see
///       [`super::ca::prepare_overlay`]).
///  2. Bind-mount the directory and cache mounts on the rootfs. Skips a
///     mount if its path has a symlink that the container user or a share
///     controls (see [`safe_resolve`]).
///     A new or empty cache dir gets the owner and mode of its target.
///  3. Hide the configured mask paths in the project.
///
/// Args:
///  - `mounts`: Mount configuration from the host
///  - `uid`: Container user ID
///  - `gid`: Container group ID
pub(super) fn assemble(mounts: &MountConfig, uid: u32, gid: u32) -> anyhow::Result<()> {
    let has_disk = Path::new("/mnt/disk").is_dir();

    // Upper/work must be on a local filesystem (not VirtioFS/FUSE). Use the
    // disk if it exists (persistent), otherwise tmpfs (not persistent).
    let (upper, work) = if has_disk {
        reset_if_image_changed(&mounts.image_id)?;
        std::fs::create_dir_all("/mnt/disk/overlay/upper")?;
        std::fs::create_dir_all("/mnt/disk/overlay/work")?;
        ("/mnt/disk/overlay/upper", "/mnt/disk/overlay/work")
    } else {
        std::fs::create_dir_all("/tmp/overlay_upper")?;
        std::fs::create_dir_all("/tmp/overlay_work")?;
        ("/tmp/overlay_upper", "/tmp/overlay_work")
    };

    // Write the file mount symlinks into the upper layer BEFORE the overlayfs
    // mount. overlayfs merges each symlink
    // upper/{target_rel} -> /airlock/.files/{rw|ro}/{mount_key} into the
    // container rootfs. The container resolves the link through
    // /airlock/.files/{rw|ro}/, which `container::setup` later bind-mounts
    // from the VirtioFS share.
    for file in &mounts.files {
        let rw_or_ro = if file.read_only { "ro" } else { "rw" };
        let link_target = format!("/airlock/.files/{rw_or_ro}/{}", file.mount_key);
        let rel = file.target.strip_prefix('/').unwrap_or(&file.target);
        let upper_path = format!("{upper}/{rel}");
        if let Some(parent) = Path::new(&upper_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Replace an existing entry at this path. The file mount has
        // priority.
        let _ = std::fs::remove_file(&upper_path);
        std::os::unix::fs::symlink(&link_target, &upper_path).map_err(|e| {
            anyhow::anyhow!("failed to create file mount symlink {upper_path} → {link_target}: {e}")
        })?;
        debug!("file symlink: {upper_path} → {link_target}");
    }

    // Keep a copy of the file links on the disk for debugging. It shows the
    // active file mounts and stays after overlay resets.
    if has_disk {
        std::fs::create_dir_all("/mnt/disk/filelinks")?;
        let current_keys: std::collections::HashSet<&str> =
            mounts.files.iter().map(|f| f.mount_key.as_str()).collect();
        let entries = std::fs::read_dir("/mnt/disk/filelinks")?;
        for entry in entries.flatten() {
            let name = entry.file_name();
            if !current_keys.contains(name.to_string_lossy().as_ref()) {
                let _ = std::fs::remove_file(entry.path());
            }
        }
        for file in &mounts.files {
            let rw_or_ro = if file.read_only { "ro" } else { "rw" };
            let link_target = format!("/airlock/.files/{rw_or_ro}/{}", file.mount_key);
            let filelink_path = format!("/mnt/disk/filelinks/{}", file.mount_key);
            let _ = std::fs::remove_file(&filelink_path);
            std::os::unix::fs::symlink(&link_target, &filelink_path).map_err(|e| {
                anyhow::anyhow!("failed to create filelink {filelink_path} → {link_target}: {e}")
            })?;
        }
    }

    // overlayfs: one rootfs tree per layer (lowerdirs, topmost first) and the
    // project state (upperdir). The project CA is an extra tmpfs lowerdir
    // above the image layers (see `ca::prepare_overlay`). Thus no CA write
    // goes to the persistent upperdir. Otherwise the CA would be added again
    // at each reboot when the upperdir persists.
    //
    // `userxattr` makes overlayfs use whiteouts encoded as `user.overlay.*`
    // xattrs. The host-side extractor keeps whiteouts in this form, because
    // it does not have CAP_MKNOD. Needs kernel >= 5.11.
    let ca_overlay = super::ca::prepare_overlay(mounts)?;

    let layer_dirs: Vec<String> = mounts
        .image_layers
        .iter()
        .map(|d| format!("/mnt/layers/{d}"))
        .collect();
    if layer_dirs.is_empty() {
        anyhow::bail!("no image layers supplied");
    }
    let mut lower_dirs: Vec<String> = Vec::with_capacity(layer_dirs.len() + 1);
    if let Some(dir) = ca_overlay {
        lower_dirs.push(dir.to_string());
    }
    lower_dirs.extend(layer_dirs.iter().cloned());
    for dir in &lower_dirs {
        debug!("overlayfs lower: {dir} exists={}", Path::new(dir).is_dir());
    }
    let lower = lower_dirs.join(":");
    // Force `index=off,xino=off`. With `index=on` (the default for RW
    // overlays), the kernel writes an origin xattr on the upperdir root. The
    // xattr is a file handle into the lower, and the kernel checks it again
    // at each mount. virtiofsd gives new inode IDs after each VM restart, so
    // the check fails on the 2nd mount with ESTALE:
    //     overlayfs: failed to verify upper root origin
    // `xino=off` is necessary for the same reason. With
    // `CONFIG_OVERLAY_FS_XINO_AUTO=y`, the kernel encodes a layer identity
    // into upper inode numbers, and this identity also becomes stale after
    // a virtiofsd restart.
    // The sandbox does not need `index`. It only keeps hard links consistent
    // across copy-ups, and the sandbox does not use that.
    let opts =
        format!("lowerdir={lower},upperdir={upper},workdir={work},userxattr,index=off,xino=off");
    info!("overlayfs opts: {opts}");
    let opts_cstr = std::ffi::CString::new(opts.as_str()).unwrap();
    let overlay_type = std::ffi::CString::new("overlay").unwrap();
    let target = std::ffi::CString::new("/mnt/overlay/rootfs").unwrap();
    let ret = unsafe {
        libc::mount(
            overlay_type.as_ptr(),
            target.as_ptr(),
            overlay_type.as_ptr(),
            0,
            opts_cstr.as_ptr().cast(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        for line in recent_kmsg_overlay_lines() {
            tracing::error!("kmsg: {line}");
        }
        anyhow::bail!("failed to mount overlayfs: {err}");
    }
    info!("assembled rootfs via overlayfs");

    let rootfs = safe_resolve::open_dir("/mnt/overlay/rootfs")?;
    // Symlinks in the shares are refused, because another sandbox or the
    // host can change them.
    let mut shared_devs = Vec::new();
    for dir in &mounts.dirs {
        let fd = safe_resolve::open_dir(&format!("/mnt/{}", dir.tag))?;
        shared_devs.push(safe_resolve::stat_at(&fd, OsStr::new(""), libc::AT_EMPTY_PATH)?.st_dev);
    }
    let policy = Policy {
        uid,
        gid,
        shared_devs,
    };

    // Directory bind mounts
    for dir in &mounts.dirs {
        let src_path = format!("/mnt/{}", dir.tag);
        let src = safe_resolve::open_dir(&src_path)?;
        let Some(dst) = resolve_or_skip(&rootfs, &dir.target, &policy)? else {
            continue;
        };
        super::mount::bind_fd(&src, &dst.fd, dir.read_only)
            .map_err(|e| anyhow::anyhow!("{src_path} → {}: {e}", dir.target))?;
        info!("dir: {src_path} → {}", dir.target);
    }

    // Cache bind mounts (after the dir mounts, so they have priority over
    // them).
    if has_disk {
        for cache in mounts.caches.iter().filter(|c| c.enabled) {
            let cache_root = safe_resolve::open_dir(&format!("/mnt/disk/cache/{}", cache.name))?;
            for target in &cache.paths {
                let Some(dst) = resolve_or_skip(&rootfs, target, &policy)? else {
                    continue;
                };
                // Paths of the same cache can be in each other, so the
                // cache dir can have symlinks of the container user.
                let Some(src) = resolve_or_skip(&cache_root, target, &policy)? else {
                    continue;
                };
                if let Err(e) = set_cache_owner(&src, &dst, uid, gid) {
                    warn!("cache {} → {target}: {e:#}", cache.name);
                }
                super::mount::bind_fd(&src.fd, &dst.fd, false)
                    .map_err(|e| anyhow::anyhow!("cache {} → {target}: {e}", cache.name))?;
                info!("cache: {} → {target}", cache.name);
            }
        }
    }

    // Hide all user-defined `[mask.<name>]` paths. Each mask has its own
    // source directory under `<mask_root>/project/<name>`, so the blocks
    // stay isolated from each other. The tree is made again at
    // each VM start, so the host config is the source of truth.
    if let Some(project_mount) = mounts.dirs.iter().find(|d| d.tag == "project") {
        let mask_root = if has_disk {
            "/mnt/disk/mask"
        } else {
            "/tmp/airlock-mask"
        };
        let project_mask_root = format!("{mask_root}/project");
        // Remove and make again the mask source tree, so a changed mask
        // gets no old state from a previous boot.
        let _ = std::fs::remove_dir_all(&project_mask_root);
        std::fs::create_dir_all(&project_mask_root)?;

        // User-defined masks: bind-mount the empty source directory of the
        // mask over each of its paths in the project. A mask hides secrets,
        // so a refused path stops the boot. It is not skipped. A guest can
        // thus stop the next boot of its own project, and that is accepted.
        // The project dir is the root of the mask paths, so a `..` or a
        // symlink can not move a mask out of the project.
        let project = safe_resolve::mkdirs(rootfs.as_fd(), &project_mount.target, &policy)
            .map_err(|e| anyhow::anyhow!("masks: {e:#}"))?;
        for mask in &mounts.masks {
            let src_path = format!("{project_mask_root}/{}", mask.name);
            std::fs::create_dir_all(&src_path)?;
            let src = safe_resolve::open_dir(&src_path)?;
            for rel in &mask.paths {
                let dst = safe_resolve::mkdirs(project.fd.as_fd(), rel, &policy)
                    .map_err(|e| anyhow::anyhow!("mask {}: {e:#}", mask.name))?;
                super::mount::bind_fd(&src, &dst.fd, true)
                    .map_err(|e| anyhow::anyhow!("mask {} → {rel}: {e}", mask.name))?;
                info!("mask {}: hid {rel}", mask.name);
            }
        }
    }

    Ok(())
}

/// Give a cache dir the owner and mode of its target. The container user
/// can then write a cache in its home dir.
///
/// The cache dir gets the owner and mode of the target dir if the target
/// exists in the rootfs. A new target and the cache dir get the container
/// user and mode 0755. The change applies only to a new or empty cache dir
/// and not to its content, so a user change of the owner stays.
/// Args:
///  - `src`: Cache dir on the project disk
///  - `dst`: Target dir in the rootfs
///  - `uid`: Container user ID
///  - `gid`: Container group ID
fn set_cache_owner(
    src: &safe_resolve::Resolved,
    dst: &safe_resolve::Resolved,
    uid: u32,
    gid: u32,
) -> anyhow::Result<()> {
    let (uid, gid, mode) = if dst.created {
        // A new target has the owner of its parent dir. Give it the owner
        // of the cache, so the next boot reads the same owner from it.
        safe_resolve::chown(&dst.fd, uid, gid)?;
        (uid, gid, 0o755)
    } else {
        let st = safe_resolve::stat_at(&dst.fd, OsStr::new(""), libc::AT_EMPTY_PATH)?;
        (st.st_uid, st.st_gid, st.st_mode & 0o7777)
    };
    // The `/proc/self/fd` link opens the exact dir of the fd.
    let src_path = format!("/proc/self/fd/{}", src.fd.as_raw_fd());
    if !src.created && std::fs::read_dir(&src_path)?.next().is_some() {
        debug!("cache dir has content, keeping its owner");
        return Ok(());
    }
    safe_resolve::chown(&src.fd, uid, gid)?;
    std::fs::set_permissions(&src_path, std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Resolve the mount target `path` below `root`.
/// Returns:
///   The directory, or `None` if the policy refuses a symlink in the path.
///   A refused path gets a warning, and the boot continues without this
///   mount.
fn resolve_or_skip(
    root: &OwnedFd,
    path: &str,
    policy: &Policy,
) -> anyhow::Result<Option<safe_resolve::Resolved>> {
    match safe_resolve::mkdirs(root.as_fd(), path, policy) {
        Ok(dir) => Ok(Some(dir)),
        Err(e) if e.is::<safe_resolve::Refused>() => {
            warn!("skipping mount: {e}");
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// Read `/dev/kmsg` without blocking and return the last 20 lines that
/// contain "overlay". Shows the kernel's own error message when `mount(2)`
/// returns a generic errno such as ESTALE.
fn recent_kmsg_overlay_lines() -> Vec<String> {
    let fd = unsafe { libc::open(c"/dev/kmsg".as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK) };
    if fd < 0 {
        return Vec::new();
    }
    let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
    let mut buf = [0u8; 4096];
    let mut lines: Vec<String> = Vec::new();
    loop {
        match file.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                let line = String::from_utf8_lossy(&buf[..n]).into_owned();
                if line.to_lowercase().contains("overlay") {
                    lines.push(line.trim_end().to_string());
                }
            }
        }
    }
    let take = lines.len().saturating_sub(20);
    lines.split_off(take)
}

/// Reset the overlay upper layer if the base image changed. Then writes the
/// new image ID to `/mnt/disk/overlay/.image_id`.
fn reset_if_image_changed(image_id: &str) -> anyhow::Result<()> {
    let id_file = "/mnt/disk/overlay/.image_id";
    // On the first run the file does not exist. This is normal. An empty
    // value causes a reset.
    let current = std::fs::read_to_string(id_file).unwrap_or_default();
    if !current.is_empty() && current.trim() == image_id {
        debug!("overlay image ID matches, keeping existing state");
        return Ok(());
    }
    info!("image changed, resetting overlay");
    if let Err(e) = std::fs::remove_dir_all("/mnt/disk/overlay") {
        debug!("overlay cleanup: {e}");
    }
    std::fs::create_dir_all("/mnt/disk/overlay")?;
    std::fs::write(id_file, image_id)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::os::fd::AsFd;
    use std::os::unix::fs::{MetadataExt, PermissionsExt, chown};
    use std::path::Path;

    use airlock_test_utils::temp_dir;

    use super::set_cache_owner;
    use crate::init::linux::safe_resolve::{Policy, Resolved, mkdirs, open_dir};

    /// Resolve the cache dir and the target dir of `path`, then set the
    /// owner of the cache dir for container user 1000:1000.
    fn set_owner(rootfs: &Path, cache: &Path, path: &str) {
        let policy = Policy {
            uid: 1000,
            gid: 1000,
            shared_devs: Vec::new(),
        };
        let resolve = |root: &Path| -> Resolved {
            let root = open_dir(root.to_str().unwrap()).unwrap();
            mkdirs(root.as_fd(), path, &policy).unwrap()
        };
        let dst = resolve(rootfs);
        let src = resolve(cache);
        set_cache_owner(&src, &dst, 1000, 1000).unwrap();
    }

    /// Get the uid, gid and mode of `path`.
    fn owner(path: &Path) -> (u32, u32, u32) {
        let m = std::fs::metadata(path).unwrap();
        (m.uid(), m.gid(), m.mode() & 0o7777)
    }

    /// Test that the owner of a cache comes from the target. Then a
    /// non-root container user can write a cache in its home dir, and a
    /// cache on an image dir has the same access as the image dir.
    /// Needs root for the chown.
    ///   1. Make an image dir owned by 1234:1234 with mode 0750
    ///   2. Set the owner of a new cache for it and for a missing target
    ///   3. Check that the first cache has the image owner and mode
    ///   4. Check that the second cache and its target have the container
    ///      user
    #[test]
    fn cache_owner_comes_from_target() {
        // SAFETY: no arguments, no side effects.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skip: needs root");
            return;
        }
        let tmp = temp_dir();
        let (rootfs, cache) = (tmp.path().join("rootfs"), tmp.path().join("cache"));
        std::fs::create_dir_all(rootfs.join("srv/data")).unwrap();
        std::fs::create_dir_all(&cache).unwrap();
        chown(rootfs.join("srv/data"), Some(1234), Some(1234)).unwrap();
        std::fs::set_permissions(rootfs.join("srv/data"), PermissionsExt::from_mode(0o750))
            .unwrap();

        set_owner(&rootfs, &cache, "/srv/data");
        set_owner(&rootfs, &cache, "/opt/new");

        assert_eq!(owner(&cache.join("srv/data")), (1234, 1234, 0o750));
        assert_eq!(owner(&cache.join("srv")), (0, 0, 0o755));
        assert_eq!(owner(&cache.join("opt/new")), (1000, 1000, 0o755));
        assert_eq!(owner(&rootfs.join("opt/new")), (1000, 1000, 0o755));
    }

    /// Test that a cache with content keeps its owner. The user can change
    /// the owner on purpose, and a change of a large cache is slow.
    /// Needs root for the chown.
    ///   1. Make a cache dir owned by root with a file in it
    ///   2. Make the target owned by 1000:1000
    ///   3. Check that the cache dir stays owned by root
    #[test]
    fn cache_with_existing_content_keeps_owner() {
        // SAFETY: no arguments, no side effects.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skip: needs root");
            return;
        }
        let tmp = temp_dir();
        let (rootfs, cache) = (tmp.path().join("rootfs"), tmp.path().join("cache"));
        std::fs::create_dir_all(rootfs.join("home/u/.cache")).unwrap();
        chown(rootfs.join("home/u/.cache"), Some(1000), Some(1000)).unwrap();
        std::fs::create_dir_all(cache.join("home/u/.cache")).unwrap();
        std::fs::write(cache.join("home/u/.cache/f"), "x").unwrap();

        set_owner(&rootfs, &cache, "/home/u/.cache");

        assert_eq!(owner(&cache.join("home/u/.cache")).0, 0);
    }
}
