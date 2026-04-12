//! Linux-specific guest VM initialization.
//!
//! This module runs once at supervisor startup to turn the raw VM into a
//! usable container environment: set the clock, mount VirtioFS shares, format
//! and mount the project disk, assemble the overlayfs rootfs, and configure
//! iptables-based networking.

use std::path::Path;
use std::process::Command;

use tracing::{debug, error, info, warn};

use super::{CacheConfig, InitConfig, MountConfig};
use crate::rpc::SocketForwardConfig;

/// Run all guest initialization steps in order, including container mounts.
///
/// The ordering matters: VirtioFS shares must be mounted before we can
/// assemble the overlay, networking must be up before the proxy starts, and
/// the project disk must be ready before the overlayfs rootfs is assembled.
/// Container mounts (proc/sys/dev, file bind mounts) run last so they take
/// precedence over earlier dir bind mounts.
pub fn setup(
    config: &InitConfig,
    mounts: &MountConfig,
    sockets: &[SocketForwardConfig],
    nested_virt: bool,
) -> anyhow::Result<()> {
    set_clock(config.epoch);

    // 1. Mount well-known VirtioFS shares
    for tag in ["base", "overlay"] {
        mount_virtiofs(tag)?;
    }

    // 2. Mount user-defined VirtioFS shares (dir mounts from RPC)
    for dir in &mounts.dirs {
        mount_virtiofs(&dir.tag)?;
    }

    // 3. Networking
    setup_networking(&config.host_ports);

    // 4. Project disk (ext4 — overlayfs upper + cache)
    setup_disk(&mounts.caches)?;

    // 5. Assemble container rootfs (overlayfs layers + dir/cache bind mounts)
    assemble_rootfs(mounts)?;

    // 6. DNS
    setup_dns()?;

    // 7. Container mounts: proc/sys/dev, socket forwards, file bind mounts.
    //    Runs after assemble_rootfs so file bind mounts can override paths
    //    inside dir-bind-mounted directories (e.g. guest_cwd).
    setup_container_mounts(mounts, sockets, nested_virt)?;

    Ok(())
}

/// Mount all filesystems that the container process needs inside its rootfs.
///
/// Called at the end of `setup()`. Handles the mounts that crun previously
/// managed via config.json: proc/sys/dev, file overlay bind mounts, socket
/// forward bind mounts, and optionally /dev/kvm.
fn setup_container_mounts(
    mounts: &MountConfig,
    sockets: &[SocketForwardConfig],
    nested_virt: bool,
) -> anyhow::Result<()> {
    let root = "/mnt/overlay/rootfs";

    // proc
    std::fs::create_dir_all(format!("{root}/proc"))?;
    mount_fs("proc", &format!("{root}/proc"), "proc", 0, "")?;

    // sysfs — writable so container runtimes (Docker) can manage cgroups
    std::fs::create_dir_all(format!("{root}/sys"))?;
    mount_fs(
        "sysfs",
        &format!("{root}/sys"),
        "sysfs",
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        "",
    )?;

    // cgroup2 — required by Docker / containerd to create and manage cgroups
    std::fs::create_dir_all(format!("{root}/sys/fs/cgroup"))?;
    mount_fs(
        "cgroup2",
        &format!("{root}/sys/fs/cgroup"),
        "cgroup2",
        0,
        "",
    )?;

    // /dev — recursive bind from VM /dev (avoids mknod; all devices already present)
    std::fs::create_dir_all(format!("{root}/dev"))?;
    bind_mount_rec("/dev", &format!("{root}/dev"))?;

    // /dev/pts
    std::fs::create_dir_all(format!("{root}/dev/pts"))?;
    mount_fs(
        "devpts",
        &format!("{root}/dev/pts"),
        "devpts",
        libc::MS_NOSUID | libc::MS_NOEXEC,
        "newinstance,ptmxmode=0666,mode=0620",
    )?;

    // /dev/shm
    std::fs::create_dir_all(format!("{root}/dev/shm"))?;
    mount_fs(
        "shm",
        &format!("{root}/dev/shm"),
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        "mode=1777,size=65536k",
    )?;

    // /ez/disk — ext4 project disk (or tmpfs fallback) exposed directly so
    // container workloads that need a non-overlayfs filesystem (e.g. Docker's
    // overlayfs snapshotter) can bind-mount a subdirectory as needed.
    std::fs::create_dir_all(format!("{root}/ez/disk"))?;
    if Path::new("/mnt/disk").is_dir() {
        std::fs::create_dir_all("/mnt/disk/userdata")?;
        bind_mount("/mnt/disk/userdata", &format!("{root}/ez/disk"), false)?;
        info!("/ez/disk → /mnt/disk/userdata (ext4)");
    } else {
        mount_fs(
            "ez-disk",
            &format!("{root}/ez/disk"),
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            "mode=0755",
        )?;
        info!("/ez/disk → tmpfs");
    }

    // Socket forward bind mounts
    // The socket files at /mnt/disk/sockets/<name> are created by net::socket::start.
    // We create placeholder files here so the bind mount succeeds; net::socket::start
    // removes and recreates the socket.
    for sock in sockets {
        let sock_name = sock.guest.rsplit('/').next().unwrap_or(sock.guest.as_str());
        let src = format!("/mnt/disk/sockets/{sock_name}");
        let dst = format!("{root}/{}", sock.guest.trim_start_matches('/'));

        std::fs::create_dir_all("/mnt/disk/sockets")?;
        // Create placeholder regular file so bind mount works
        if !Path::new(&src).exists() {
            let _ = std::fs::File::create(&src);
        }
        if let Some(parent) = Path::new(&dst).parent() {
            std::fs::create_dir_all(parent)?;
        }
        if !Path::new(&dst).exists() {
            let _ = std::fs::File::create(&dst);
        }
        bind_mount(&src, &dst, false)?;
        info!("socket: {src} → {dst}");
    }

    // File mounts — expose files_rw and files_ro as directories inside the container,
    // then create symlinks at the target paths pointing into those directories.
    //
    // VirtioFS DIRECTORY bind mounts work correctly. VirtioFS FILE bind mounts fail with
    // EACCES on open() despite correct permissions. Symlinks through a VirtioFS directory
    // avoid the per-file bind mount entirely while keeping writes live (symlink → VirtioFS
    // → host file).
    std::fs::create_dir_all(format!("{root}/ez/.files/rw"))?;
    std::fs::create_dir_all(format!("{root}/ez/.files/ro"))?;
    bind_mount(
        "/mnt/overlay/files_rw",
        &format!("{root}/ez/.files/rw"),
        false,
    )?;
    bind_mount(
        "/mnt/overlay/files_ro",
        &format!("{root}/ez/.files/ro"),
        true,
    )?;
    for file in &mounts.files {
        let subdir = if file.read_only { "ro" } else { "rw" };
        let rel = file.target.strip_prefix('/').unwrap_or(&file.target);
        let link_target = format!("/ez/.files/{subdir}/{rel}");
        let dst = format!("{root}/{rel}");
        if let Some(parent) = Path::new(&dst).parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Remove any stale entry (file, anchor, or old symlink with different target).
        let _ = std::fs::remove_file(&dst);
        std::os::unix::fs::symlink(&link_target, &dst).map_err(|e| {
            anyhow::anyhow!("failed to create file mount symlink {dst} → {link_target}: {e}")
        })?;
        info!("file: {dst} → {link_target}");
    }

    // /dev/kvm for nested virtualization (already in /dev bind, but explicit for clarity)
    if nested_virt && !Path::new("/dev/kvm").exists() {
        warn!("/dev/kvm requested but not present in VM");
    }

    info!("container mounts configured");
    Ok(())
}

/// Assemble the container rootfs from overlayfs layers, file symlinks,
/// directory bind mounts, and cache bind mounts.
fn assemble_rootfs(mounts: &MountConfig) -> anyhow::Result<()> {
    let has_disk = Path::new("/mnt/disk").is_dir();

    // Upper/work must be on a local filesystem (not VirtioFS/FUSE).
    // Use disk if available (persists), otherwise tmpfs (ephemeral).
    let (upper, work) = if has_disk {
        reset_overlay_if_needed(&mounts.image_id);
        std::fs::create_dir_all("/mnt/disk/overlay/rootfs")
            .unwrap_or_else(|e| error!("overlay rootfs dir: {e}"));
        std::fs::create_dir_all("/mnt/disk/overlay/work")
            .unwrap_or_else(|e| error!("overlay work dir: {e}"));
        ("/mnt/disk/overlay/rootfs", "/mnt/disk/overlay/work")
    } else {
        std::fs::create_dir_all("/tmp/overlay_rootfs")
            .unwrap_or_else(|e| error!("overlay rootfs dir: {e}"));
        std::fs::create_dir_all("/tmp/overlay_work")
            .unwrap_or_else(|e| error!("overlay work dir: {e}"));
        ("/tmp/overlay_rootfs", "/tmp/overlay_work")
    };

    // overlayfs: ca layer + base image (lowerdirs) + project state (upperdir)
    let ca_dir = Path::new("/mnt/overlay/ca");
    let ca_exists = ca_dir.is_dir();
    debug!("overlayfs ca layer: exists={ca_exists}");
    let lower = if ca_exists {
        "/mnt/overlay/ca:/mnt/base".to_string()
    } else {
        "/mnt/base".to_string()
    };
    let opts = format!("lowerdir={lower},upperdir={upper},workdir={work}");
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
        anyhow::bail!("failed to mount overlayfs: {err}");
    }
    info!("assembled rootfs via overlayfs");

    // Directory bind mounts
    for dir in &mounts.dirs {
        let src = format!("/mnt/{}", dir.tag);
        let rel = dir.target.strip_prefix('/').unwrap_or(&dir.target);
        let dst = Path::new("/mnt/overlay/rootfs").join(rel);
        let dst = dst.to_string_lossy();
        std::fs::create_dir_all(dst.as_ref())?;
        bind_mount(&src, &dst, dir.read_only)?;
        info!("dir: {src} → {dst}");
    }

    // Cache bind mounts (last — override dir mounts).
    if has_disk {
        for cache in mounts.caches.iter().filter(|c| c.enabled) {
            for target in &cache.paths {
                let rel = target.strip_prefix('/').unwrap_or(target);
                let src = Path::new("/mnt/disk/cache").join(&cache.name).join(rel);
                let dst = Path::new("/mnt/overlay/rootfs").join(rel);
                let (src, dst) = (src.to_string_lossy(), dst.to_string_lossy());
                std::fs::create_dir_all(src.as_ref())?;
                std::fs::create_dir_all(dst.as_ref())?;
                bind_mount(&src, &dst, false)?;
                info!("cache: {} → {dst}", &cache.name);
            }
        }
    }

    Ok(())
}

/// Reset the overlay upper layer if the base image changed.
fn reset_overlay_if_needed(image_id: &str) {
    let id_file = "/mnt/disk/overlay/.image_id";
    let current = std::fs::read_to_string(id_file).unwrap_or_default();
    if !current.is_empty() && current.trim() == image_id {
        debug!("overlay image ID matches, keeping existing state");
        return;
    }
    info!("image changed, resetting overlay");
    if let Err(e) = std::fs::remove_dir_all("/mnt/disk/overlay") {
        debug!("overlay cleanup: {e}");
    }
    if let Err(e) = std::fs::create_dir_all("/mnt/disk/overlay") {
        error!("failed to create overlay dir: {e}");
    }
    if let Err(e) = std::fs::write(id_file, image_id) {
        error!("failed to write image ID: {e}");
    }
}

/// Mount a VirtioFS share by its tag name at `/mnt/<tag>`.
fn mount_virtiofs(tag: &str) -> anyhow::Result<()> {
    let mount_point = format!("/mnt/{tag}");
    std::fs::create_dir_all(&mount_point)?;
    let tag_cstr = std::ffi::CString::new(tag).unwrap();
    let mount_cstr = std::ffi::CString::new(mount_point.as_str()).unwrap();
    let fstype = std::ffi::CString::new("virtiofs").unwrap();
    let ret = unsafe {
        libc::mount(
            tag_cstr.as_ptr(),
            mount_cstr.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to mount virtiofs {tag}: {err}");
    }
    debug!("mounted virtiofs: {tag} → {mount_point}");
    Ok(())
}

/// Create a bind mount using the `mount(2)` syscall directly.
fn bind_mount(src: &str, dst: &str, read_only: bool) -> anyhow::Result<()> {
    let src_cstr = std::ffi::CString::new(src).unwrap();
    let dst_cstr = std::ffi::CString::new(dst).unwrap();
    let flags = if read_only {
        libc::MS_BIND | libc::MS_RDONLY
    } else {
        libc::MS_BIND
    };
    let ret = unsafe {
        libc::mount(
            src_cstr.as_ptr(),
            dst_cstr.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to bind-mount {src} → {dst}: {err}");
    }
    Ok(())
}

/// Recursive bind mount (MS_BIND | MS_REC).
fn bind_mount_rec(src: &str, dst: &str) -> anyhow::Result<()> {
    let src_cstr = std::ffi::CString::new(src).unwrap();
    let dst_cstr = std::ffi::CString::new(dst).unwrap();
    let ret = unsafe {
        libc::mount(
            src_cstr.as_ptr(),
            dst_cstr.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to recursive bind-mount {src} → {dst}: {err}");
    }
    Ok(())
}

/// Mount a filesystem with optional data string.
fn mount_fs(
    source: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: &str,
) -> anyhow::Result<()> {
    let src_cstr = std::ffi::CString::new(source).unwrap();
    let dst_cstr = std::ffi::CString::new(target).unwrap();
    let fs_cstr = std::ffi::CString::new(fstype).unwrap();
    // Leak the CString to keep the pointer valid across the syscall
    let data_ptr = if data.is_empty() {
        std::ptr::null()
    } else {
        let c = std::ffi::CString::new(data).unwrap();
        let p = c.as_ptr().cast::<libc::c_void>();
        std::mem::forget(c);
        p
    };
    let ret = unsafe {
        libc::mount(
            src_cstr.as_ptr(),
            dst_cstr.as_ptr(),
            fs_cstr.as_ptr(),
            flags,
            data_ptr,
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to mount {fstype} at {target}: {err}");
    }
    debug!("mounted {fstype} at {target}");
    Ok(())
}

/// Set the guest system clock from the host-provided epoch.
fn set_clock(epoch: u64) {
    if epoch == 0 {
        return;
    }
    let ts = libc::timespec {
        tv_sec: epoch as i64,
        tv_nsec: 0,
    };
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &raw const ts) } != 0 {
        warn!("failed to set system clock");
    } else {
        debug!("system clock set to epoch {epoch}");
    }
}

/// Configure loopback networking and iptables rules.
fn setup_networking(host_ports: &[u16]) {
    run_quiet(&["/sbin/ip", "link", "set", "lo", "up"]);

    write_sysctl("/proc/sys/net/ipv4/conf/lo/route_localnet", "1");
    write_sysctl("/proc/sys/net/ipv4/conf/all/rp_filter", "0");
    write_sysctl("/proc/sys/net/ipv4/conf/lo/rp_filter", "0");
    write_sysctl("/proc/sys/net/ipv4/ip_forward", "1");

    run_quiet(&["/sbin/ip", "addr", "add", "10.0.0.1/8", "dev", "lo"]);
    run_quiet(&[
        "/sbin/ip", "route", "add", "default", "via", "10.0.0.1", "dev", "lo",
    ]);

    for port in host_ports {
        run_quiet(&[
            "/usr/sbin/iptables",
            "-t",
            "nat",
            "-A",
            "OUTPUT",
            "-p",
            "tcp",
            "-d",
            "127.0.0.1",
            "--dport",
            &port.to_string(),
            "-j",
            "REDIRECT",
            "--to-port",
            "15001",
        ]);
    }
    run_quiet(&[
        "/usr/sbin/iptables",
        "-t",
        "nat",
        "-A",
        "OUTPUT",
        "-p",
        "tcp",
        "-d",
        "127.0.0.1",
        "-j",
        "RETURN",
    ]);
    run_quiet(&[
        "/usr/sbin/iptables",
        "-t",
        "nat",
        "-A",
        "OUTPUT",
        "-p",
        "tcp",
        "--dport",
        "15001",
        "-j",
        "RETURN",
    ]);
    run_quiet(&[
        "/usr/sbin/iptables",
        "-t",
        "nat",
        "-A",
        "OUTPUT",
        "-p",
        "tcp",
        "-j",
        "REDIRECT",
        "--to-port",
        "15001",
    ]);

    info!("networking configured");
}

/// Mount the project disk at /mnt/disk (overlay upper + cache).
fn setup_disk(cache_mounts: &[CacheConfig]) -> anyhow::Result<()> {
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
            .args(["-q", "-E", "nodiscard", "-L", "ezpez-disk", dev])
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
    if let Ok(entries) = std::fs::read_dir("/mnt/disk/cache") {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !known_names.contains(name.as_ref()) {
                debug!("removing stale cache dir: {name}");
                if let Err(e) = std::fs::remove_dir_all(entry.path()) {
                    warn!("failed to remove stale cache {name}: {e}");
                }
            }
        }
    }

    for cache in cache_mounts {
        std::fs::create_dir_all(format!("/mnt/disk/cache/{}", cache.name))?;
    }
    Ok(())
}

/// Point the container's `/etc/resolv.conf` at the in-VM DNS server.
fn setup_dns() -> anyhow::Result<()> {
    let dir = "/mnt/overlay/rootfs/etc";
    std::fs::create_dir_all(dir)?;
    std::fs::write(format!("{dir}/resolv.conf"), "nameserver 10.0.0.1\n")?;
    Ok(())
}

fn write_sysctl(path: &str, value: &str) {
    if let Err(e) = std::fs::write(path, value) {
        debug!("sysctl {path}={value} failed: {e}");
    }
}

fn run_quiet(args: &[&str]) {
    let cmd_str = args.join(" ");
    match Command::new(args[0]).args(&args[1..]).output() {
        Ok(output) if !output.status.success() => {
            error!(
                "{cmd_str}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(e) => error!("{cmd_str}: exec failed: {e}"),
        Ok(_) => debug!("{cmd_str}: ok"),
    }
}
