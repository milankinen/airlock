//! Assemble the container rootfs at `/mnt/overlay/rootfs` by composing
//! the OCI image layers (topmost-first, as `lowerdir`s) with an
//! upperdir on the project disk (or tmpfs when no disk is present).
//!
//! Before mounting the overlay this also:
//! - writes file-mount symlinks into the upperdir (their targets
//!   resolve through the `/airlock/.files/{rw|ro}/` bind mount
//!   installed later by `container::setup`),
//! - stages per-file CA bundles on a tmpfs lowerdir via `ca::prepare_overlay`,
//! - resets the upperdir when the image digest changes so stale
//!   upperdir paths from a previous image don't shadow the new one.
//!
//! Once overlayfs is up, this also wires dir/cache bind mounts on top
//! of the composed rootfs and masks `.airlock/` so the container can't
//! reach back into sandbox internals (CA keys, disk image, lock file).

use std::io::Read;
use std::os::fd::FromRawFd;
use std::path::Path;

use tracing::{debug, info};

use crate::init::MountConfig;

/// Assemble the container rootfs from overlayfs layers, file symlinks,
/// directory bind mounts, and cache bind mounts.
pub(super) fn assemble(mounts: &MountConfig) -> anyhow::Result<()> {
    let has_disk = Path::new("/mnt/disk").is_dir();

    // Upper/work must be on a local filesystem (not VirtioFS/FUSE).
    // Use disk if available (persists), otherwise tmpfs (ephemeral).
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

    // Write file mount symlinks into the upper layer BEFORE mounting overlayfs.
    // Each symlink at upper/{target_rel} → /airlock/.files/{rw|ro}/{mount_key}
    // is merged into the container rootfs by overlayfs. The container resolves
    // the path through /airlock/.files/{rw|ro}/ which is bind-mounted from the
    // VirtioFS share (set up in setup_container_mounts).
    for file in &mounts.files {
        let rw_or_ro = if file.read_only { "ro" } else { "rw" };
        let link_target = format!("/airlock/.files/{rw_or_ro}/{}", file.mount_key);
        let rel = file.target.strip_prefix('/').unwrap_or(&file.target);
        let upper_path = format!("{upper}/{rel}");
        if let Some(parent) = Path::new(&upper_path).parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Overwrite any existing entry at this path (file mount takes precedence).
        let _ = std::fs::remove_file(&upper_path);
        std::os::unix::fs::symlink(&link_target, &upper_path).map_err(|e| {
            anyhow::anyhow!("failed to create file mount symlink {upper_path} → {link_target}: {e}")
        })?;
        debug!("file symlink: {upper_path} → {link_target}");
    }

    // Persist filelinks for debugging (survives overlay resets, shows active file mounts).
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

    // overlayfs: per-layer rootfs trees (lowerdirs, topmost-first) +
    // project state (upperdir). The project CA is staged as an extra tmpfs
    // lowerdir placed on top of the image layers (see `ca::prepare_overlay`),
    // so CA writes never land on the persistent upperdir — without that, the
    // appended CA would accumulate across reboots when the upperdir is kept.
    //
    // `userxattr` makes overlayfs honor whiteouts encoded as `user.overlay.*`
    // xattrs, which is how the host-side extractor preserves whiteouts without
    // needing CAP_MKNOD. Requires kernel >= 5.11.
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
    // overlays) the kernel records a file-handle-based origin xattr on the
    // upperdir root pointing into the lower, then re-verifies it on every
    // remount. virtiofsd assigns fresh inode ids across VM restarts, so the
    // verification fails on the 2nd mount with ESTALE:
    //     overlayfs: failed to verify upper root origin
    // `xino=off` matters for the same reason — with `CONFIG_OVERLAY_FS_XINO_AUTO=y`
    // the kernel would otherwise encode a layer identity into upper inode
    // numbers that likewise goes stale after a virtiofsd restart.
    // We don't need `index` (only used for hardlink consistency across
    // copy-ups, which we don't rely on).
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

    // Cache bind mounts (last — override dir mounts).
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

    // Mask .airlock/: bind an empty read-only directory over the sandbox
    // directory's .airlock/ so the container user cannot accidentally read or
    // modify sandbox internals (CA keys, disk image, lock file, etc.).
    if let Some(project_mount) = mounts.dirs.iter().find(|d| d.tag == "project") {
        let mask_src = if has_disk {
            std::fs::create_dir_all("/mnt/disk/mask")?;
            "/mnt/disk/mask"
        } else {
            std::fs::create_dir_all("/tmp/airlock-mask")?;
            "/tmp/airlock-mask"
        };
        let dst = crate::util::resolve_in_root(rootfs, &project_mount.target).join(".airlock");
        std::fs::create_dir_all(&dst)?;
        super::mount::bind(mask_src, &dst.to_string_lossy(), true)?;
        info!("masked .airlock at {}", dst.display());
    }

    Ok(())
}

/// Drain `/dev/kmsg` non-blocking and return the most recent lines that
/// mention "overlay". Used to surface the kernel's own error message when
/// `mount(2)` returns a generic errno like ESTALE.
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

/// Reset the overlay upper layer if the base image changed.
fn reset_if_image_changed(image_id: &str) -> anyhow::Result<()> {
    let id_file = "/mnt/disk/overlay/.image_id";
    // Missing file is normal on first run — treat as empty (needs reset).
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
