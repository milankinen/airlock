//! Project disk setup: format `/dev/vda` as ext4 on first boot, mount
//! it at `/mnt/disk`, and materialize one subdirectory per configured
//! cache mount. The disk backs the overlayfs upperdir and all
//! persistent caches; on subsequent boots we skip the format and just
//! mount what's already there.

use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

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

    // Bias VFS reclaim toward dentry/inode slab over the page cache.
    // The host virtiofs proxy holds an FD open for every cached
    // dentry/inode in the guest; with the default 100 + airlock's
    // half-host-RAM allocation, those FDs accumulate into the
    // hundreds of thousands and bump into macOS's process FD ceiling.
    // 200 doubles the relative reclaim weight without affecting
    // anything else; the periodic `drop_caches` below does the
    // forced eviction.
    if let Err(e) = std::fs::write("/proc/sys/vm/vfs_cache_pressure", "200") {
        warn!("sysctl vm.vfs_cache_pressure=200 failed: {e}");
    }

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

/// `FITRIM` ioctl — `_IOWR('X', 121, struct fstrim_range)` on Linux.
/// Hardcoded because libc doesn't expose it; the encoding is the same
/// across the architectures airlock targets. The numeric type is
/// `libc::Ioctl`, which is `c_ulong` on glibc and `c_int` on musl —
/// `as` casts down to whichever the current libc expects.
const FITRIM: libc::Ioctl = 0xc018_5879_u32 as libc::Ioctl;

/// Matches `struct fstrim_range` from `<linux/fs.h>`. `start = 0`,
/// `len = u64::MAX` ("to end-of-fs"), `minlen = 0` (kernel default).
#[repr(C)]
struct FstrimRange {
    start: u64,
    len: u64,
    minlen: u64,
}

/// Run two periodic cleanups every 10 minutes for the lifetime of
/// the supervisor:
///
/// 1. `FITRIM` on `/mnt/disk` — the project disk image is sparse but
///    doesn't reclaim space when files are deleted inside the
///    sandbox; ext4 tracks already-trimmed extents so steady-state
///    invocations after the working set settles are near-no-ops, but
///    the host file shrinks live as the user deletes files instead
///    of staying inflated until exit.
///
/// 2. `echo 2 > /proc/sys/vm/drop_caches` — releases the kernel
///    dentry/inode slab. The host virtiofs proxy keeps an FD open
///    for every cached entry; without forced reclaim those FDs
///    accumulate over a long session into the hundreds of thousands
///    and start tripping macOS's process FD limit. Dropping only
///    slab (`2`, not `3`) keeps the page cache intact, so file
///    *contents* aren't re-fetched from the host — only metadata
///    walks are re-stat'd on next access.
///
/// Errors are logged and swallowed — both are best-effort.
pub fn start_periodic_maintenance() {
    tokio::task::spawn_local(async {
        let interval = Duration::from_mins(10);
        // First fire is one interval after boot rather than at boot
        // itself — at boot there's typically nothing to discard yet
        // and no slab to drop.
        let mut ticker = tokio::time::interval_at(tokio::time::Instant::now() + interval, interval);
        loop {
            ticker.tick().await;
            match trim_once("/mnt/disk") {
                Ok(bytes) => debug!("fstrim /mnt/disk: {bytes} bytes trimmed"),
                Err(e) => warn!("fstrim /mnt/disk failed: {e}"),
            }
            match drop_slab_caches() {
                Ok(()) => debug!("drop_caches=2 issued"),
                Err(e) => warn!("drop_caches=2 failed: {e}"),
            }
        }
    });
}

fn trim_once(path: &str) -> std::io::Result<u64> {
    let dir = std::fs::File::open(path)?;
    let mut range = FstrimRange {
        start: 0,
        len: u64::MAX,
        minlen: 0,
    };
    // SAFETY: `range` is a valid `fstrim_range` for the FITRIM ioctl;
    // the kernel writes the trimmed-byte count back into `len`.
    let ret = unsafe { libc::ioctl(dir.as_raw_fd(), FITRIM, &raw mut range) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(range.len)
}

/// Write `2` to `/proc/sys/vm/drop_caches` — releases dentry/inode
/// slab without touching the page cache.
fn drop_slab_caches() -> std::io::Result<()> {
    std::fs::write("/proc/sys/vm/drop_caches", "2")
}
