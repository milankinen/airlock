//! Linux-specific guest VM initialization.
//!
//! This module runs once at supervisor startup to turn the raw VM into a
//! usable container environment: set the clock, mount VirtioFS shares, format
//! and mount the project disk, assemble the overlayfs rootfs, and configure
//! iptables-based networking.

use std::path::Path;
use std::process::Command;

use tracing::{debug, info, warn};

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
    _sockets: &[SocketForwardConfig],
    nested_virt: bool,
) -> anyhow::Result<()> {
    set_clock(config.epoch, config.epoch_nanos);

    // 1. Mount well-known VirtioFS shares
    mount_virtiofs("layers")?;

    // 2. Mount user dir shares (includes "project" and "dir_N" mounts)
    for dir in &mounts.dirs {
        mount_virtiofs(&dir.tag)?;
    }

    // 3. Mount file-mount VirtioFS shares (present only if config has file mounts)
    if mounts.files.iter().any(|f| !f.read_only) {
        mount_virtiofs("files/rw")?;
    }
    if mounts.files.iter().any(|f| f.read_only) {
        mount_virtiofs("files/ro")?;
    }

    // Create local directory for the overlayfs mount point (no longer a VirtioFS share).
    std::fs::create_dir_all("/mnt/overlay/rootfs")?;

    // 4. Networking
    setup_networking(&config.host_ports)?;

    // 5. Project disk (ext4 — overlayfs upper + cache)
    setup_disk(&mounts.caches)?;

    // 6. Assemble container rootfs (overlayfs layers + dir/cache bind mounts)
    assemble_rootfs(mounts)?;

    // 7. DNS
    setup_dns()?;

    // 8. Container mounts: proc/sys/dev, file bind mounts.
    //    Runs after assemble_rootfs so file bind mounts can override paths
    //    inside dir-bind-mounted directories (e.g. guest_cwd).
    setup_container_mounts(mounts, nested_virt)?;

    Ok(())
}

/// Mount all filesystems that the container process needs inside its rootfs.
///
/// Called at the end of `setup()`. Handles the mounts that crun previously
/// managed via config.json: proc/sys/dev, file overlay bind mounts, socket
/// forward bind mounts, and optionally /dev/kvm.
fn setup_container_mounts(mounts: &MountConfig, nested_virt: bool) -> anyhow::Result<()> {
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

    // /airlock/disk — ext4 project disk (or tmpfs fallback) exposed directly so
    // container workloads that need a non-overlayfs filesystem (e.g. Docker's
    // overlayfs snapshotter) can bind-mount a subdirectory as needed.
    std::fs::create_dir_all(format!("{root}/airlock/disk"))?;
    if Path::new("/mnt/disk").is_dir() {
        std::fs::create_dir_all("/mnt/disk/userdata")?;
        bind_mount("/mnt/disk/userdata", &format!("{root}/airlock/disk"), false)?;
        info!("/airlock/disk → /mnt/disk/userdata (ext4)");
    } else {
        mount_fs(
            "airlock-disk",
            &format!("{root}/airlock/disk"),
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            "mode=0755",
        )?;
        info!("/airlock/disk → tmpfs");
    }

    // File mounts: bind the VirtioFS files shares into the container so that the
    // symlinks placed in the upper layer (by assemble_rootfs) can be resolved.
    // The symlinks point to /airlock/.files/{rw|ro}/{mount_key}, which resolves
    // through this bind mount to /mnt/files/{rw|ro}/{mount_key} — the hard-linked
    // source file in the project overlay directory.
    if mounts.files.iter().any(|f| !f.read_only) {
        let dst = format!("{root}/airlock/.files/rw");
        std::fs::create_dir_all(&dst)?;
        bind_mount("/mnt/files/rw", &dst, false)?;
        info!("/airlock/.files/rw → /mnt/files/rw");
    }
    if mounts.files.iter().any(|f| f.read_only) {
        let dst = format!("{root}/airlock/.files/ro");
        std::fs::create_dir_all(&dst)?;
        bind_mount("/mnt/files/ro", &dst, true)?;
        info!("/airlock/.files/ro → /mnt/files/ro");
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
        reset_overlay_if_needed(&mounts.image_id)?;
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
    // lowerdir placed on top of the image layers (see `prepare_ca_overlay`),
    // so CA writes never land on the persistent upperdir — without that, the
    // appended CA would accumulate across reboots when the upperdir is kept.
    //
    // `userxattr` makes overlayfs honor whiteouts encoded as `user.overlay.*`
    // xattrs, which is how the host-side extractor preserves whiteouts without
    // needing CAP_MKNOD. Requires kernel >= 5.11.
    let ca_overlay = prepare_ca_overlay(mounts)?;

    let layer_dirs: Vec<String> = mounts
        .image_layers
        .iter()
        .map(|d| format!("/mnt/layers/{d}/rootfs"))
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
    let opts = format!("lowerdir={lower},upperdir={upper},workdir={work},userxattr");
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

    let rootfs = Path::new("/mnt/overlay/rootfs");

    // Directory bind mounts
    for dir in &mounts.dirs {
        let src = format!("/mnt/{}", dir.tag);
        let dst = crate::util::resolve_in_root(rootfs, &dir.target);
        std::fs::create_dir_all(&dst)?;
        bind_mount(&src, &dst.to_string_lossy(), dir.read_only)?;
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
                bind_mount(&src.to_string_lossy(), &dst.to_string_lossy(), false)?;
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
        bind_mount(mask_src, &dst.to_string_lossy(), true)?;
        info!("masked .airlock at {}", dst.display());
    }

    Ok(())
}

/// Reset the overlay upper layer if the base image changed.
fn reset_overlay_if_needed(image_id: &str) -> anyhow::Result<()> {
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

/// Mount a VirtioFS share by its tag name at `/mnt/<tag>`.
fn mount_virtiofs(tag: &str) -> anyhow::Result<()> {
    let mount_point = format!("/mnt/{tag}");
    mount_virtiofs_at(tag, &mount_point)
}

/// Mount a VirtioFS share by its tag name at an arbitrary path.
fn mount_virtiofs_at(tag: &str, mount_point: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(mount_point)?;
    let tag_cstr = std::ffi::CString::new(tag).unwrap();
    let mount_cstr = std::ffi::CString::new(mount_point).unwrap();
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
        anyhow::bail!("failed to mount virtiofs {tag} at {mount_point}: {err}");
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
fn set_clock(epoch: u64, epoch_nanos: u32) {
    if epoch == 0 {
        return;
    }
    let ts = libc::timespec {
        tv_sec: epoch as i64,
        tv_nsec: i64::from(epoch_nanos),
    };
    if unsafe { libc::clock_settime(libc::CLOCK_REALTIME, &raw const ts) } != 0 {
        warn!("failed to set system clock");
    } else {
        debug!("system clock set to epoch {epoch}.{epoch_nanos:09}");
    }
}

/// Configure loopback networking and iptables rules.
fn setup_networking(host_ports: &[u16]) -> anyhow::Result<()> {
    run_cmd(&["/sbin/ip", "link", "set", "lo", "up"])?;

    write_sysctl("/proc/sys/net/ipv4/conf/lo/route_localnet", "1")?;
    write_sysctl("/proc/sys/net/ipv4/conf/all/rp_filter", "0")?;
    write_sysctl("/proc/sys/net/ipv4/conf/lo/rp_filter", "0")?;
    write_sysctl("/proc/sys/net/ipv4/ip_forward", "1")?;

    run_cmd(&["/sbin/ip", "addr", "add", "10.0.0.1/8", "dev", "lo"])?;
    run_cmd(&[
        "/sbin/ip", "route", "add", "default", "via", "10.0.0.1", "dev", "lo",
    ])?;

    for port in host_ports {
        run_cmd(&[
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
        ])?;
    }
    run_cmd(&[
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
    ])?;
    run_cmd(&[
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
    ])?;
    run_cmd(&[
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
    ])?;

    info!("networking configured");
    Ok(())
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

/// Point the container's `/etc/resolv.conf` at the in-VM DNS server.
fn setup_dns() -> anyhow::Result<()> {
    let dir = "/mnt/overlay/rootfs/etc";
    std::fs::create_dir_all(dir)?;
    std::fs::write(format!("{dir}/resolv.conf"), "nameserver 10.0.0.1\n")?;
    Ok(())
}

fn write_sysctl(path: &str, value: &str) -> anyhow::Result<()> {
    std::fs::write(path, value).map_err(|e| anyhow::anyhow!("sysctl {path}={value} failed: {e}"))
}

fn run_cmd(args: &[&str]) -> anyhow::Result<()> {
    let cmd_str = args.join(" ");
    let output = Command::new(args[0])
        .args(&args[1..])
        .output()
        .map_err(|e| anyhow::anyhow!("{cmd_str}: exec failed: {e}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{cmd_str}: {}", stderr.trim());
    }
    debug!("{cmd_str}: ok");
    Ok(())
}

/// CA bundle paths known across common distros. Each path is relative to the
/// rootfs. Guest init merges the project CA into each existing bundle (read
/// from the image's lower layers) and falls back to writing the Debian/Ubuntu
/// path when none are present, so `SSL_CERT_FILE` can point at a predictable
/// location in minimal images.
const CA_BUNDLE_PATHS: &[&str] = &[
    "etc/ssl/certs/ca-certificates.crt", // Debian/Ubuntu/Alpine
    "etc/ssl/cert.pem",                  // Alpine/LibreSSL
    "etc/pki/tls/certs/ca-bundle.crt",   // RHEL/CentOS/Fedora
    "etc/ssl/ca-bundle.pem",             // openSUSE/SLES
    "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem", // RHEL/Fedora
];

/// Drop-in anchor locations for distro trust-update tools. When the user or
/// a package postinst runs `update-ca-certificates` / `update-ca-trust` /
/// `trust extract-compat` it rebuilds the bundles from these directories —
/// so shipping the project CA as a plain file here makes it survive any
/// future rebuild of `etc/ssl/certs/ca-certificates.crt` on the upperdir.
const CA_ANCHOR_PATHS: &[&str] = &[
    "usr/local/share/ca-certificates/airlock.crt", // Debian/Ubuntu/Alpine — update-ca-certificates
    "etc/pki/ca-trust/source/anchors/airlock.crt", // RHEL/Fedora/CentOS — update-ca-trust
    "etc/pki/trust/anchors/airlock.crt",           // openSUSE/SLES — update-ca-certificates
    "etc/ca-certificates/trust-source/anchors/airlock.crt", // Arch — trust extract-compat
];

/// tmpfs lowerdir holding pre-merged CA bundles. Placed above the image
/// layers in the overlayfs stack so the project CA is visible without any
/// write ever landing on the persistent upperdir.
const CA_OVERLAY_DIR: &str = "/mnt/ca-overlay";

/// Build a tmpfs lowerdir containing per-bundle copies of every CA bundle the
/// image ships, each with the project CA appended. Returns the tmpfs path
/// when anything was written (so the caller can splice it into `lowerdir`),
/// or `None` when there's no project CA to inject.
///
/// This runs **before** overlayfs is mounted: for each well-known bundle path
/// we walk `image_layers` topmost-first, take the first layer that ships a
/// non-empty copy of that file, append the project CA, and drop the result
/// into the tmpfs at the same relative path. Doing the merge against the
/// pristine layer content — not the already-merged overlayfs view — is what
/// prevents the CA from accumulating across reboots when the upperdir is
/// persisted on the project disk.
fn prepare_ca_overlay(mounts: &MountConfig) -> anyhow::Result<Option<&'static str>> {
    if mounts.ca_cert.is_empty() {
        return Ok(None);
    }
    std::fs::create_dir_all(CA_OVERLAY_DIR)?;
    mount_fs(
        "ca-overlay",
        CA_OVERLAY_DIR,
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        "mode=0755",
    )?;

    let mut wrote_any = false;
    for rel in CA_BUNDLE_PATHS {
        let Some(base) = find_bundle_in_layers(&mounts.image_layers, rel)? else {
            continue;
        };
        write_ca_bundle(rel, &base, &mounts.ca_cert)?;
        wrote_any = true;
        debug!("ca: merged /{rel} from image layers");
    }
    if !wrote_any {
        write_ca_bundle(CA_BUNDLE_PATHS[0], &[], &mounts.ca_cert)?;
        debug!(
            "ca: wrote fallback /{} (no CA bundle shipped by image)",
            CA_BUNDLE_PATHS[0]
        );
    }

    // Drop the raw CA into every well-known anchor directory so trust-update
    // tools regenerate bundles that still include it. Cheap and harmless when
    // the tool isn't installed — the file just sits there unread.
    for rel in CA_ANCHOR_PATHS {
        write_ca_bundle(rel, &[], &mounts.ca_cert)?;
        debug!("ca: dropped anchor /{rel}");
    }
    Ok(Some(CA_OVERLAY_DIR))
}

/// Find the first layer that ships `rel` (topmost-first) and return its
/// contents. An empty file in a layer is treated as "masked here" — either an
/// overlayfs whiteout placeholder from our extractor or a deliberately empty
/// bundle — and stops the walk so we don't resurrect content the image meant
/// to hide. `None` means no layer had the path at all.
fn find_bundle_in_layers(layers: &[String], rel: &str) -> anyhow::Result<Option<Vec<u8>>> {
    for digest in layers {
        let path = Path::new("/mnt/layers")
            .join(digest)
            .join("rootfs")
            .join(rel);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() && meta.len() > 0 => {
                return Ok(Some(std::fs::read(&path)?));
            }
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

fn write_ca_bundle(rel: &str, base: &[u8], ca_cert: &[u8]) -> anyhow::Result<()> {
    let target = Path::new(CA_OVERLAY_DIR).join(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = base.to_vec();
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(ca_cert);
    std::fs::write(&target, &out)
        .map_err(|e| anyhow::anyhow!("write CA bundle {}: {e}", target.display()))
}
