//! Container rootfs.
//!
//! Makes the container rootfs from the image layers, the project state and the
//! configured mounts. Hides sandbox internals and the masked paths from the
//! container.

use std::io::Read;
use std::os::fd::FromRawFd;
use std::path::Path;

use tracing::{debug, info};

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
///  2. Bind-mount the directory and cache mounts on the rootfs.
///  3. Hide `.airlock/` and the configured mask paths, so the container
///     cannot access sandbox internals (CA keys, disk image, lock file).
///
/// Args:
///  - `mounts`: Mount configuration from the host
pub(super) fn assemble(mounts: &MountConfig) -> anyhow::Result<()> {
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

    let rootfs = Path::new("/mnt/overlay/rootfs");

    // Directory bind mounts
    for dir in &mounts.dirs {
        let src = format!("/mnt/{}", dir.tag);
        let dst = crate::util::resolve_in_root(rootfs, &dir.target);
        std::fs::create_dir_all(&dst)?;
        super::mount::bind(&src, &dst.to_string_lossy(), dir.read_only)?;
        info!("dir: {src} → {}", dst.display());
    }

    // Cache bind mounts (after the dir mounts, so they have priority over
    // them).
    if has_disk {
        for cache in mounts.caches.iter().filter(|c| c.enabled) {
            for target in &cache.paths {
                let rel = target.strip_prefix('/').unwrap_or(target);
                let src = Path::new("/mnt/disk/cache").join(&cache.name).join(rel);
                let dst = crate::util::resolve_in_root(rootfs, target);
                std::fs::create_dir_all(&src)?;
                std::fs::create_dir_all(&dst)?;
                super::mount::bind(&src.to_string_lossy(), &dst.to_string_lossy(), false)?;
                info!("cache: {} → {}", &cache.name, dst.display());
            }
        }
    }

    // Hide .airlock/ and all user-defined `[mask.<name>]` paths. Each mask
    // has its own source directory under `<mask_root>/project/<name>`, so
    // the blocks stay isolated from each other. The tree is made again at
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

        let project_root = crate::util::resolve_in_root(rootfs, &project_mount.target);

        // Built-in: hide the .airlock/ directory of the sandbox, so the
        // container cannot read CA keys, disk image, lock file and others.
        {
            let src = format!("{project_mask_root}/.airlock");
            std::fs::create_dir_all(&src)?;
            let dst = project_root.join(".airlock");
            std::fs::create_dir_all(&dst)?;
            super::mount::bind(&src, &dst.to_string_lossy(), true)?;
            info!("masked .airlock at {}", dst.display());
        }

        // User-defined masks: bind-mount the empty source directory of the
        // mask over each of its paths in the project.
        for mask in &mounts.masks {
            let src = format!("{project_mask_root}/{}", mask.name);
            std::fs::create_dir_all(&src)?;
            for rel in &mask.paths {
                let dst = project_root.join(rel);
                std::fs::create_dir_all(&dst)?;
                super::mount::bind(&src, &dst.to_string_lossy(), true)?;
                info!("mask {}: hid {}", mask.name, dst.display());
            }
        }
    }

    Ok(())
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
