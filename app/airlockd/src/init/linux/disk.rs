//! Project disk.
//!
//! The project disk keeps the changes to the container rootfs and all
//! persistent caches. Prepares the disk at boot. At intervals, gives unused
//! disk space and kernel caches back to the host.

use std::os::unix::io::AsRawFd;
use std::path::Path;
use std::process::Command;
use std::time::Duration;

use tracing::{debug, info, warn};

use crate::init::CacheConfig;

/// Mount the project disk at `/mnt/disk` (overlay upper layer and caches).
///
/// Formats `/dev/vda` as ext4 on the first boot. Later boots only mount the
/// disk. Creates one directory per cache under `/mnt/disk/cache` and removes
/// the directories of caches that are not in the config.
/// Args:
///  - `cache_mounts`: Cache mounts from the config
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
            // The disk state is unknown. A format here would erase an
            // existing filesystem, with the overlay upper layer and all
            // caches. Thus "unknown" means "do not format". If the disk
            // really has no filesystem, the mount below fails with an error.
            // This is better than data loss without an error.
            warn!("blkid exec failed ({e}); not formatting to avoid data loss");
            false
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

    // Make VFS reclaim prefer the dentry/inode slab to the page cache.
    // The host virtiofs proxy keeps an FD open for each cached dentry/inode
    // in the guest. With the default value 100 and a VM that has half of
    // the host RAM, the number of FDs can grow to hundreds of thousands and
    // reach the macOS process FD limit. The value 200 doubles the relative
    // reclaim weight and has no other effect. The periodic `drop_caches`
    // (see `start_periodic_maintenance`) forces the eviction.
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

/// `FITRIM` ioctl number: `_IOWR('X', 121, struct fstrim_range)` on Linux.
///
/// Hardcoded, because libc does not have it. The encoding is the same on
/// all architectures that airlock supports. The type `libc::Ioctl` is
/// `c_ulong` on glibc and `c_int` on musl. The `as` cast converts the value
/// to the type of the current libc.
const FITRIM: libc::Ioctl = 0xc018_5879_u32 as libc::Ioctl;

/// Same layout as `struct fstrim_range` from `<linux/fs.h>`. Use
/// `start = 0`, `len = u64::MAX` (to the end of the filesystem) and
/// `minlen = 0` (kernel default).
#[repr(C)]
struct FstrimRange {
    start: u64,
    len: u64,
    minlen: u64,
}

/// Start a task that does two cleanups every 10 minutes, for the lifetime
/// of the supervisor:
///
/// 1. `FITRIM` on `/mnt/disk`. The project disk image is sparse, but it
///    does not give space back when the sandbox deletes files. With the
///    trim, the host file becomes smaller while the user deletes files. It
///    does not stay large until exit. ext4 remembers the extents that it
///    already trimmed, so later trims of a stable disk do almost nothing.
///
/// 2. `echo 2 > /proc/sys/vm/drop_caches`. Releases the kernel dentry/inode
///    slab. The host virtiofs proxy keeps an FD open for each cached entry.
///    Without a forced release, a long session can collect hundreds of
///    thousands of these FDs and reach the macOS process FD limit. Only
///    the slab is dropped (`2`, not `3`), so the page cache stays. File
///    *contents* are not read again from the host. Only metadata is read
///    again on the next access.
///
/// Both cleanups are best-effort. Errors are logged and then ignored.
pub fn start_periodic_maintenance() {
    tokio::task::spawn_local(async {
        let interval = Duration::from_mins(10);
        // The first run is one interval after boot, not at boot. At boot
        // there is usually nothing to trim and no slab to drop.
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

/// Run `FITRIM` once on the filesystem at `path`. Returns the number of
/// trimmed bytes.
fn trim_once(path: &str) -> std::io::Result<u64> {
    let dir = std::fs::File::open(path)?;
    let mut range = FstrimRange {
        start: 0,
        len: u64::MAX,
        minlen: 0,
    };
    // SAFETY: `range` is a valid `fstrim_range` for the FITRIM ioctl. The
    // kernel writes the number of trimmed bytes back into `len`.
    let ret = unsafe { libc::ioctl(dir.as_raw_fd(), FITRIM, &raw mut range) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(range.len)
}

/// Write `2` to `/proc/sys/vm/drop_caches`. Releases the dentry/inode slab
/// and keeps the page cache.
fn drop_slab_caches() -> std::io::Result<()> {
    std::fs::write("/proc/sys/vm/drop_caches", "2")
}
