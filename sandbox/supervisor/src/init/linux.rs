use std::path::Path;
use std::process::Command;

use tracing::{debug, error, info, warn};

use super::InitConfig;

/// Mount config read from /mnt/overlay/mounts.json (written by host).
#[derive(serde::Deserialize)]
struct MountsConfig {
    #[serde(default)]
    image_id: String,
    #[serde(default)]
    dirs: Vec<DirMount>,
    #[serde(default)]
    files: Vec<FileMount>,
    #[serde(default)]
    cache: Vec<String>,
}

#[derive(serde::Deserialize)]
struct DirMount {
    tag: String,
    target: String,
    read_only: bool,
}

#[derive(serde::Deserialize)]
struct FileMount {
    target: String,
    read_only: bool,
}

pub fn setup(config: &InitConfig) -> anyhow::Result<()> {
    set_clock(config.epoch);

    // 1. Mount well-known VirtioFS shares
    for tag in ["base", "overlay"] {
        mount_virtiofs(tag)?;
    }

    // 2. Read mount config
    let mounts = read_mounts_config()?;

    // 3. Mount user-defined VirtioFS shares
    for dir in &mounts.dirs {
        mount_virtiofs(&dir.tag)?;
    }

    // 4. Networking
    setup_networking(&config.host_ports);

    // 5. Project disk (ext4 — overlayfs upper + cache)
    setup_disk(&mounts.cache);

    // 6. Assemble container rootfs
    assemble_rootfs(&mounts)?;

    // 7. DNS
    setup_dns();

    Ok(())
}

fn read_mounts_config() -> anyhow::Result<MountsConfig> {
    let path = "/mnt/overlay/mounts.json";
    let data =
        std::fs::read_to_string(path).map_err(|e| anyhow::anyhow!("failed to read {path}: {e}"))?;
    serde_json::from_str(&data).map_err(|e| anyhow::anyhow!("failed to parse {path}: {e}"))
}

/// Assemble the container rootfs from overlayfs layers, file symlinks,
/// directory bind mounts, and cache bind mounts.
fn assemble_rootfs(mounts: &MountsConfig) -> anyhow::Result<()> {
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

    // overlayfs: base image (lowerdir) + project state (upperdir)
    let opts = format!("lowerdir=/mnt/base,upperdir={upper},workdir={work}");
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

    // Mount points for file mount directories (used by OCI config)
    let _ = std::fs::create_dir_all("/mnt/overlay/rootfs/.ez/files_rw");
    let _ = std::fs::create_dir_all("/mnt/overlay/rootfs/.ez/files_ro");

    // File mounts: symlinks into /.ez/files_rw or /.ez/files_ro.
    // VirtioFS doesn't support file-level bind mounts (data reads fail
    // with EACCES), but directory bind mounts work. The OCI config binds
    // the files_rw/files_ro dirs to /.ez/, and symlinks point there.
    for file in &mounts.files {
        let subdir = if file.read_only {
            "files_ro"
        } else {
            "files_rw"
        };
        let rel = file.target.strip_prefix('/').unwrap_or(&file.target);
        let link = Path::new("/mnt/overlay/rootfs").join(rel);
        let symlink_target = format!("/.ez/{subdir}/{rel}");
        if let Some(parent) = link.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(&symlink_target, &link).map_err(|e| {
            anyhow::anyhow!(
                "failed to symlink {} → {symlink_target}: {e}",
                link.display()
            )
        })?;
        debug!("file symlink: {rel} → {symlink_target}");
    }

    // Directory bind mounts
    for dir in &mounts.dirs {
        let src = format!("/mnt/{}", dir.tag);
        let rel = dir.target.strip_prefix('/').unwrap_or(&dir.target);
        let dst = Path::new("/mnt/overlay/rootfs").join(rel);
        let dst = dst.to_string_lossy();
        std::fs::create_dir_all(dst.as_ref())?;
        bind_mount(&src, &dst, dir.read_only)?;
        debug!("dir mount: {src} → {dst}");
    }

    // Cache bind mounts (last — override dir mounts)
    if has_disk {
        for target in &mounts.cache {
            let rel = target.strip_prefix('/').unwrap_or(target);
            let src = Path::new("/mnt/disk/cache").join(rel);
            let dst = Path::new("/mnt/overlay/rootfs").join(rel);
            let (src, dst) = (src.to_string_lossy(), dst.to_string_lossy());
            std::fs::create_dir_all(src.as_ref())?;
            std::fs::create_dir_all(dst.as_ref())?;
            bind_mount(&src, &dst, false)?;
            debug!("cache mount: {src} → {dst}");
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
fn setup_disk(cache_dirs: &[String]) {
    let dev = "/dev/vda";
    if !Path::new(dev).exists() {
        error!("disk {dev} not found");
        return;
    }

    let blkid = Command::new("/sbin/blkid").arg(dev).output();
    let needs_format = match &blkid {
        Ok(o) => {
            let out = String::from_utf8_lossy(&o.stdout);
            debug!("blkid {dev}: {out}");
            !out.contains("ext4")
        }
        Err(e) => {
            error!("blkid exec failed: {e}");
            true
        }
    };

    if needs_format {
        info!("formatting disk {dev}");
        match Command::new("/sbin/mkfs.ext4")
            .args(["-q", "-L", "ezpez-disk", dev])
            .status()
        {
            Ok(s) if s.success() => debug!("formatted {dev}"),
            Ok(s) => {
                error!("mkfs.ext4 failed: {s}");
                return;
            }
            Err(e) => {
                error!("mkfs.ext4 exec failed: {e}");
                return;
            }
        }
    }

    let _ = std::fs::create_dir_all("/mnt/disk");
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
        error!("failed to mount {dev}: {err}");
        return;
    }
    info!("mounted disk at /mnt/disk");
    let _ = Command::new("/usr/sbin/resize2fs").arg(dev).output();

    // Create cache directories
    for dir in cache_dirs {
        let rel = dir.strip_prefix('/').unwrap_or(dir);
        let _ = std::fs::create_dir_all(format!("/mnt/disk/cache/{rel}"));
    }
}

fn setup_dns() {
    let dir = "/mnt/overlay/rootfs/etc";
    if let Err(e) = std::fs::create_dir_all(dir) {
        error!("failed to create {dir}: {e}");
        return;
    }
    if let Err(e) = std::fs::write(format!("{dir}/resolv.conf"), "nameserver 10.0.0.1\n") {
        error!("failed to write resolv.conf: {e}");
    }
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
