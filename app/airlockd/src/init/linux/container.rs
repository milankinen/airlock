//! Container-internal mounts: everything the OCI runtime would
//! normally set up from `config.json` (proc/sys/dev, cgroup2,
//! devpts/shm, file bind mounts) but that we install directly so
//! crun's mount logic stays out of the hot path.
//!
//! Runs **after** `overlay::assemble` so file-mount bind mounts can
//! override paths inside dir-bind-mounted directories, and so the
//! overlayfs rootfs exists at `/mnt/overlay/rootfs` to be populated.

use std::path::Path;

use tracing::{debug, info, warn};

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

    // /dev — a fresh tmpfs populated with only the standard device nodes.
    //
    // We deliberately do NOT recursively bind the VM's /dev: that exposed
    // block devices like /dev/vda (the raw ext4 project disk) to the
    // container, letting a container process read/write around the overlay
    // view and the file-mask bind mounts (which only hide paths at the
    // filesystem layer) — defeating the guest-side masking guarantees.
    //
    // Instead we mirror the OCI runtime default device set. Each node is
    // bind-mounted from the VM's /dev so we avoid hardcoding major/minor;
    // block/VM devices are simply never bound. MS_NODEV is intentionally
    // omitted so the bound char devices work.
    let dev = format!("{root}/dev");
    std::fs::create_dir_all(&dev)?;
    super::mount::fs("dev", &dev, "tmpfs", libc::MS_NOSUID, "mode=0755")?;

    // Standard char devices, plus /dev/fuse when present (BuildKit / rootless
    // overlay tooling). Absent nodes are skipped.
    for node in ["null", "zero", "full", "random", "urandom", "tty", "fuse"] {
        bind_dev_node(&dev, node)?;
    }
    // /dev/kvm only for nested virtualization.
    if nested_virt {
        if Path::new("/dev/kvm").exists() {
            bind_dev_node(&dev, "kvm")?;
        } else {
            warn!("/dev/kvm requested but not present in VM");
        }
    }

    // Standard /dev symlinks the runtime would create.
    for (link, target) in [
        ("fd", "/proc/self/fd"),
        ("stdin", "/proc/self/fd/0"),
        ("stdout", "/proc/self/fd/1"),
        ("stderr", "/proc/self/fd/2"),
    ] {
        let path = format!("{dev}/{link}");
        let _ = std::fs::remove_file(&path);
        std::os::unix::fs::symlink(target, &path)?;
    }

    // /dev/pts
    std::fs::create_dir_all(format!("{root}/dev/pts"))?;
    super::mount::fs(
        "devpts",
        &format!("{root}/dev/pts"),
        "devpts",
        libc::MS_NOSUID | libc::MS_NOEXEC,
        "newinstance,ptmxmode=0666,mode=0620",
    )?;

    // /dev/ptmx → the new devpts instance's ptmx (runtime default).
    let ptmx = format!("{root}/dev/ptmx");
    let _ = std::fs::remove_file(&ptmx);
    std::os::unix::fs::symlink("pts/ptmx", &ptmx)?;

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

    info!("container mounts configured");
    Ok(())
}

/// Expose a single device node from the VM's `/dev` into the container's
/// `/dev` by bind-mounting it onto a freshly created file. This mirrors what
/// `mknod` would do without needing to know the node's major/minor, and keeps
/// block/VM devices (never named here) out of the container. A node absent
/// from the VM is skipped.
fn bind_dev_node(dev_root: &str, name: &str) -> anyhow::Result<()> {
    let src = format!("/dev/{name}");
    if !Path::new(&src).exists() {
        debug!("/dev/{name} not present in VM; skipping");
        return Ok(());
    }
    let dst = format!("{dev_root}/{name}");
    std::fs::File::create(&dst)?;
    super::mount::bind(&src, &dst, false)?;
    Ok(())
}
