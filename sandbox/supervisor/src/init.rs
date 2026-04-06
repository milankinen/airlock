use std::process::Command;

use tracing::{debug, error, info, warn};

pub struct InitConfig {
    pub shares: Vec<String>,
    pub epoch: u64,
    pub host_ports: Vec<u16>,
    pub has_cache_disk: bool,
    pub cache_dirs: Vec<String>,
}

pub fn setup(config: &InitConfig) {
    set_clock(config.epoch);
    mount_virtiofs(&config.shares);
    setup_networking(&config.host_ports);
    if config.has_cache_disk {
        setup_cache_disk(&config.cache_dirs);
    }
    overlay_file_mounts();
    setup_dns();
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

fn mount_virtiofs(shares: &[String]) {
    for tag in shares {
        let mount_point = format!("/mnt/{tag}");
        if let Err(e) = std::fs::create_dir_all(&mount_point) {
            warn!("failed to create {mount_point}: {e}");
            continue;
        }
        let tag_cstr = std::ffi::CString::new(tag.as_str()).unwrap();
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
            warn!("failed to mount virtiofs {tag} at {mount_point}");
        } else {
            debug!("mounted virtiofs: {tag} → {mount_point}");
        }
    }
}

fn setup_networking(host_ports: &[u16]) {
    // Enable loopback
    run_quiet(&["ip", "link", "set", "lo", "up"]);

    // Enable routing through loopback for transparent proxy
    write_sysctl("/proc/sys/net/ipv4/conf/lo/route_localnet", "1");
    write_sysctl("/proc/sys/net/ipv4/conf/all/rp_filter", "0");
    write_sysctl("/proc/sys/net/ipv4/conf/lo/rp_filter", "0");
    write_sysctl("/proc/sys/net/ipv4/ip_forward", "1");

    // Add routable address to lo
    run_quiet(&["ip", "addr", "add", "10.0.0.1/8", "dev", "lo"]);
    run_quiet(&[
        "ip", "route", "add", "default", "via", "10.0.0.1", "dev", "lo",
    ]);

    // Host port forwarding
    for port in host_ports {
        run_quiet(&[
            "iptables",
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
    // Skip remaining localhost traffic
    run_quiet(&[
        "iptables",
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
    // Skip proxy's own port
    run_quiet(&[
        "iptables", "-t", "nat", "-A", "OUTPUT", "-p", "tcp", "--dport", "15001", "-j", "RETURN",
    ]);
    // Redirect all other outbound TCP to proxy
    run_quiet(&[
        "iptables",
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

fn setup_cache_disk(cache_dirs: &[String]) {
    let dev = "/dev/vda";
    if !std::path::Path::new(dev).exists() {
        warn!("cache disk {dev} not found, skipping");
        return;
    }

    // Check if already formatted
    let needs_format = Command::new("blkid")
        .arg(dev)
        .output()
        .map(|o| !String::from_utf8_lossy(&o.stdout).contains("ext4"))
        .unwrap_or(true);

    if needs_format {
        info!("formatting cache disk {dev}");
        let status = Command::new("mkfs.ext4")
            .args(["-q", "-L", "ezpez-cache", dev])
            .status();
        match status {
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

    if let Err(e) = std::fs::create_dir_all("/mnt/cache") {
        error!("failed to create /mnt/cache: {e}");
        return;
    }

    let dev_cstr = std::ffi::CString::new(dev).unwrap();
    let mount_cstr = std::ffi::CString::new("/mnt/cache").unwrap();
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
        error!("failed to mount {dev} at /mnt/cache");
        return;
    }
    info!("mounted cache disk at /mnt/cache");

    // Resize to fill the disk (no-op if already full size)
    let _ = Command::new("resize2fs").arg(dev).output();

    // Create cache subdirectories
    for dir in cache_dirs {
        let path = format!("/mnt/cache/{dir}");
        if let Err(e) = std::fs::create_dir_all(&path) {
            warn!("failed to create cache dir {path}: {e}");
        }
    }
}

/// Overlay file mounts onto the container rootfs.
///
/// 1. overlayfs makes files visible at their target paths. A tmpfs
///    upperdir absorbs stray rootfs writes so they don't leak to host.
/// 2. Individual bind mounts from VirtioFS on top of the overlay ensure
///    writes to mounted files sync back to the host.
fn overlay_file_mounts() {
    let has_rw = std::path::Path::new("/mnt/files_rw").is_dir();
    let has_ro = std::path::Path::new("/mnt/files_ro").is_dir();
    if !has_rw && !has_ro {
        return;
    }

    let _ = std::fs::create_dir_all("/tmp/overlay_upper");
    let _ = std::fs::create_dir_all("/tmp/overlay_work");

    // Build lowerdir chain: files_rw:files_ro:rootfs
    let mut lower = String::from("/mnt/bundle/rootfs");
    if has_ro {
        lower = format!("/mnt/files_ro:{lower}");
    }
    if has_rw {
        lower = format!("/mnt/files_rw:{lower}");
    }

    let opts = format!("lowerdir={lower},upperdir=/tmp/overlay_upper,workdir=/tmp/overlay_work");
    let opts_cstr = std::ffi::CString::new(opts.as_str()).unwrap();
    let overlay = std::ffi::CString::new("overlay").unwrap();
    let target = std::ffi::CString::new("/mnt/bundle/rootfs").unwrap();
    let ret = unsafe {
        libc::mount(
            overlay.as_ptr(),
            target.as_ptr(),
            overlay.as_ptr(),
            0,
            opts_cstr.as_ptr().cast(),
        )
    };
    if ret != 0 {
        error!("failed to mount overlay on rootfs");
        return;
    }
    info!("overlaid file mounts onto rootfs");

    // Bind-mount each rw file from VirtioFS so writes go back to host
    if has_rw {
        bind_mount_files(std::path::Path::new("/mnt/files_rw"), "/mnt/bundle/rootfs");
    }
}

fn bind_mount_files(src_root: &std::path::Path, dst_root: &str) {
    let Ok(entries) = std::fs::read_dir(src_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            bind_mount_files(&path, dst_root);
        } else if path.is_file() {
            let rel = path.strip_prefix("/mnt/files_rw").unwrap();
            let dst = format!("{dst_root}/{}", rel.display());
            let src_cstr = std::ffi::CString::new(path.to_string_lossy().as_ref()).unwrap();
            let dst_cstr = std::ffi::CString::new(dst.as_str()).unwrap();
            let ret = unsafe {
                libc::mount(
                    src_cstr.as_ptr(),
                    dst_cstr.as_ptr(),
                    std::ptr::null(),
                    libc::MS_BIND,
                    std::ptr::null(),
                )
            };
            if ret != 0 {
                warn!("failed to bind-mount {}", rel.display());
            } else {
                debug!("bind-mounted file: {}", rel.display());
            }
        }
    }
}

fn setup_dns() {
    let resolv_dir = "/mnt/bundle/rootfs/etc";
    if let Err(e) = std::fs::create_dir_all(resolv_dir) {
        warn!("failed to create {resolv_dir}: {e}");
        return;
    }
    if let Err(e) = std::fs::write(format!("{resolv_dir}/resolv.conf"), "nameserver 10.0.0.1\n") {
        warn!("failed to write resolv.conf: {e}");
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
            warn!(
                "{cmd_str}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            );
        }
        Err(e) => warn!("{cmd_str}: exec failed: {e}"),
        Ok(_) => debug!("{cmd_str}: ok"),
    }
}
