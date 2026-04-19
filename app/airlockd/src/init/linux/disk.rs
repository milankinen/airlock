//! Project disk setup: format `/dev/vda` as ext4 on first boot, mount
//! it at `/mnt/disk`, and materialize one subdirectory per configured
//! cache mount. The disk backs the overlayfs upperdir and all
//! persistent caches; on subsequent boots we skip the format and just
//! mount what's already there.

use std::path::Path;
use std::process::Command;

use tracing::{debug, info, warn};

use crate::init::CacheConfig;

/// Mount the project disk at /mnt/disk (overlay upper + cache).
pub(super) fn setup(cache_mounts: &[CacheConfig]) -> anyhow::Result<()> {
    let dev = "/dev/vda";
    if !Path::new(dev).exists() {
        anyhow::bail!("disk {dev} not found");
    }

    let blkid = Command::new("/sbin/blkid").arg(dev).output();
    let needs_format = match &blkid {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            debug!("blkid {dev}: {out}");
            !out.contains("ext4")
        }
        Err(e) => {
            warn!("blkid exec failed: {e}");
            true
        }
    };

    if needs_format {
        info!("formatting disk {dev}");
        let output = Command::new("/sbin/mkfs.ext4")
            .args(["-q", "-E", "nodiscard", "-L", "airlock-disk", dev])
            .output()
            .map_err(|e| anyhow::anyhow!("mkfs.ext4 exec failed: {e}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("mkfs.ext4 failed: {} {}", output.status, stderr.trim());
        }
        debug!("formatted {dev}");
    }

    std::fs::create_dir_all("/mnt/disk")?;
    let dev_cstr = std::ffi::CString::new(dev).unwrap();
    let mount_cstr = std::ffi::CString::new("/mnt/disk").unwrap();
    let fstype = std::ffi::CString::new("ext4").unwrap();
    let ret = unsafe {
        libc::mount(
            dev_cstr.as_ptr(),
            mount_cstr.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to mount {dev}: {err}");
    }
    info!("mounted disk at /mnt/disk");
    let _ = Command::new("/usr/sbin/resize2fs").arg(dev).output();

    std::fs::create_dir_all("/mnt/disk/cache")?;

    // Remove cache dirs for names no longer in config.
    let known_names: std::collections::HashSet<&str> =
        cache_mounts.iter().map(|c| c.name.as_str()).collect();
    for entry in std::fs::read_dir("/mnt/disk/cache")? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !known_names.contains(name.as_ref()) {
            debug!("removing stale cache dir: {name}");
            std::fs::remove_dir_all(entry.path())?;
        }
    }

    for cache in cache_mounts {
        std::fs::create_dir_all(format!("/mnt/disk/cache/{}", cache.name))?;
    }
    Ok(())
}
