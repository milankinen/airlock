//! Container-internal mounts: everything the OCI runtime would
//! normally set up from `config.json` (proc/sys/dev, cgroup2,
//! devpts/shm, file bind mounts) but that we install directly so
//! crun's mount logic stays out of the hot path.
//!
//! Runs **after** `overlay::assemble` so file-mount bind mounts can
//! override paths inside dir-bind-mounted directories, and so the
//! overlayfs rootfs exists at `/mnt/overlay/rootfs` to be populated.

use std::path::Path;

use tracing::{info, warn};

use crate::init::MountConfig;

/// Mount all filesystems that the container process needs inside its rootfs.
pub(super) fn setup(mounts: &MountConfig, nested_virt: bool) -> anyhow::Result<()> {
    let root = "/mnt/overlay/rootfs";

    // proc
    std::fs::create_dir_all(format!("{root}/proc"))?;
    super::mount::fs("proc", &format!("{root}/proc"), "proc", 0, "")?;

    // sysfs — writable so container runtimes (Docker) can manage cgroups
    std::fs::create_dir_all(format!("{root}/sys"))?;
    super::mount::fs(
        "sysfs",
        &format!("{root}/sys"),
        "sysfs",
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        "",
    )?;

    // cgroup2 — required by Docker / containerd to create and manage cgroups
    std::fs::create_dir_all(format!("{root}/sys/fs/cgroup"))?;
    super::mount::fs(
        "cgroup2",
        &format!("{root}/sys/fs/cgroup"),
        "cgroup2",
        0,
        "",
    )?;

    // /dev — recursive bind from VM /dev (avoids mknod; all devices already present)
    std::fs::create_dir_all(format!("{root}/dev"))?;
    super::mount::bind_rec("/dev", &format!("{root}/dev"))?;

    // /dev/pts
    std::fs::create_dir_all(format!("{root}/dev/pts"))?;
    super::mount::fs(
        "devpts",
        &format!("{root}/dev/pts"),
        "devpts",
        libc::MS_NOSUID | libc::MS_NOEXEC,
        "newinstance,ptmxmode=0666,mode=0620",
    )?;

    // /dev/shm
    std::fs::create_dir_all(format!("{root}/dev/shm"))?;
    super::mount::fs(
        "shm",
        &format!("{root}/dev/shm"),
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        "mode=1777,size=65536k",
    )?;

    // /tmp — a dedicated tmpfs so /tmp is not part of the overlayfs rootfs.
    // BuildKit's containerd image store mounts its transient overlay at
    // /tmp/containerd-mount*; if /tmp inherits the outer overlay's
    // userxattr/xattr semantics the differ fails with EOPNOTSUPP on
    // security.capability reads. A plain tmpfs side-steps that. noexec
    // is intentionally omitted — build tools execute scripts from /tmp.
    std::fs::create_dir_all(format!("{root}/tmp"))?;
    super::mount::fs(
        "tmp",
        &format!("{root}/tmp"),
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        "mode=1777",
    )?;

    // /airlock/disk — ext4 project disk (or tmpfs fallback) exposed directly so
    // container workloads that need a non-overlayfs filesystem (e.g. Docker's
    // overlayfs snapshotter) can bind-mount a subdirectory as needed.
    std::fs::create_dir_all(format!("{root}/airlock/disk"))?;
    if Path::new("/mnt/disk").is_dir() {
        std::fs::create_dir_all("/mnt/disk/userdata")?;
        super::mount::bind("/mnt/disk/userdata", &format!("{root}/airlock/disk"), false)?;
        info!("/airlock/disk → /mnt/disk/userdata (ext4)");
    } else {
        super::mount::fs(
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
        super::mount::bind("/mnt/files/rw", &dst, false)?;
        info!("/airlock/.files/rw → /mnt/files/rw");
    }
    if mounts.files.iter().any(|f| f.read_only) {
        let dst = format!("{root}/airlock/.files/ro");
        std::fs::create_dir_all(&dst)?;
        super::mount::bind("/mnt/files/ro", &dst, true)?;
        info!("/airlock/.files/ro → /mnt/files/ro");
    }

    // /dev/kvm for nested virtualization (already in /dev bind, but explicit for clarity)
    if nested_virt && !Path::new("/dev/kvm").exists() {
        warn!("/dev/kvm requested but not present in VM");
    }

    info!("container mounts configured");
    Ok(())
}
