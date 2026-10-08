//! System mounts in the container.
//!
//! Mounts the system filesystems that container processes need, for example
//! `/proc`, `/sys` and `/dev`, and the file mounts from the config. Usually an
//! OCI runtime does these mounts. airlockd does them itself to make the
//! container start faster.

use std::path::Path;

use tracing::{debug, info, warn};

use crate::init::MountConfig;

/// `airlock_common::BRIDGE_DIR`, relative to the container rootfs.
const BRIDGE_REL: &str = "run/airlock";

/// Mount all filesystems that the container process needs inside its rootfs.
///
/// Must run **after** `overlay::assemble`. Then the overlayfs rootfs exists
/// at `/mnt/overlay/rootfs`, and file-mount bind mounts can replace paths
/// inside directory bind mounts.
///
/// An OCI runtime usually does these mounts from `config.json`. airlockd
/// does them itself, so the mount logic of crun is not in the hot path.
/// Args:
///  - `mounts`: Mount configuration from the host
///  - `nested_virt`: Give the container access to `/dev/kvm`
pub(super) fn setup(mounts: &MountConfig, nested_virt: bool) -> anyhow::Result<()> {
    let root = "/mnt/overlay/rootfs";

    // proc
    std::fs::create_dir_all(format!("{root}/proc"))?;
    super::mount::fs("proc", &format!("{root}/proc"), "proc", 0, "")?;

    // sysfs: writable, so container runtimes (Docker) can manage cgroups
    std::fs::create_dir_all(format!("{root}/sys"))?;
    super::mount::fs(
        "sysfs",
        &format!("{root}/sys"),
        "sysfs",
        libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        "",
    )?;

    // cgroup2: Docker and containerd need it to create and manage cgroups
    std::fs::create_dir_all(format!("{root}/sys/fs/cgroup"))?;
    super::mount::fs(
        "cgroup2",
        &format!("{root}/sys/fs/cgroup"),
        "cgroup2",
        0,
        "",
    )?;

    // /dev: a new tmpfs with only the standard device nodes.
    //
    // Do NOT bind the VM's /dev recursively. That gives the container
    // access to block devices such as /dev/vda (the raw ext4 project disk).
    // A container process can then read and write data outside the overlay
    // view and the mask bind mounts, and thus get past the guest-side masks.
    //
    // Use the default device set of the OCI runtime instead. Each node is a
    // bind mount from the VM's /dev, so no major/minor numbers are
    // hardcoded. Block and VM devices are never bound. MS_NODEV is not set
    // on purpose, so the bound char devices work.
    let dev = format!("{root}/dev");
    std::fs::create_dir_all(&dev)?;
    super::mount::fs("dev", &dev, "tmpfs", libc::MS_NOSUID, "mode=0755")?;

    // Standard char devices, and /dev/fuse if it exists (for BuildKit and
    // rootless overlay tools). Nodes that do not exist are skipped.
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

    // Standard /dev symlinks that the runtime usually makes.
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

    // /dev/ptmx -> ptmx of the new devpts instance (runtime default).
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

    // /tmp: its own tmpfs, so /tmp is not part of the overlayfs rootfs.
    // The containerd image store of BuildKit mounts its temporary overlay
    // at /tmp/containerd-mount*. If /tmp has the userxattr/xattr semantics
    // of the outer overlay, the differ fails with EOPNOTSUPP when it reads
    // security.capability. A plain tmpfs prevents this. noexec is not set
    // on purpose, because build tools run scripts from /tmp.
    std::fs::create_dir_all(format!("{root}/tmp"))?;
    super::mount::fs(
        "tmp",
        &format!("{root}/tmp"),
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        "mode=1777",
    )?;

    // /run/airlock: a tmpfs for host-bridge FIFOs and shims (clipboard,
    // browser), new at each boot. Thus no bridge file persists in the
    // overlay upper layer.
    //
    // The guest controls the upper layer. This mount runs in the VM, not in
    // the sandbox root, so a symlink at /run or /run/airlock could send the
    // mount to a path that the guest selects. Thus a symlink at one of them
    // skips the tmpfs. The bridges still work then: they resolve their paths with
    // chroot semantics (`bridge::in_rootfs`). Only their files persist in
    // the upper layer.
    if let Err(e) = refuse_symlinks(Path::new(root), BRIDGE_REL) {
        warn!("/run/airlock tmpfs skipped: {e:#}");
    } else {
        let bridge = format!("{root}/{BRIDGE_REL}");
        std::fs::create_dir_all(&bridge)?;
        super::mount::fs(
            "airlock-run",
            &bridge,
            "tmpfs",
            libc::MS_NOSUID | libc::MS_NODEV,
            "mode=0755",
        )?;
    }

    // /airlock/disk: the ext4 project disk (or tmpfs if there is no disk),
    // directly available to the container. Workloads that need a
    // filesystem that is not overlayfs (for example Docker's overlayfs
    // snapshotter) can bind-mount a subdirectory of it.
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

    // File mounts: bind the VirtioFS files shares into the container, so the
    // symlinks in the upper layer (from `overlay::assemble`) resolve. The
    // symlinks point to /airlock/.files/{rw|ro}/{mount_key}. Through this
    // bind mount, that path resolves to /mnt/files/{rw|ro}/{mount_key}: the
    // hard-linked source file in the project overlay directory.
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

/// Return an error if an existing prefix of `rel` below `root` is a symlink.
///
/// Uses `symlink_metadata` on the raw path, so it finds a symlink and does
/// not follow it. Components that do not exist are accepted, because the
/// caller creates them as plain directories.
fn refuse_symlinks(root: &Path, rel: &str) -> anyhow::Result<()> {
    let mut path = root.to_path_buf();
    for component in Path::new(rel).components() {
        path.push(component);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_symlink() => {
                anyhow::bail!("refusing to mount over symlink {}", path.display());
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => anyhow::bail!("stat {}: {e}", path.display()),
        }
    }
    Ok(())
}

/// Make one device node of the VM's `/dev` available in the container's
/// `/dev`. Bind-mounts the node onto a new empty file. Does nothing if the
/// VM does not have the node.
///
/// This has the same result as `mknod`, but needs no major/minor numbers.
/// Callers never give block or VM devices, so they stay out of the
/// container.
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

#[cfg(test)]
mod tests {
    //! Tests of the symlink check for the bridge directory of the container.

    use super::*;
    use crate::test_cfg::temp_dir;

    /// Test that the symlink check accepts a bridge directory path of plain
    /// or missing directories. The caller creates the missing parts.
    ///   1. Check an empty root
    ///   2. Create `run` and check again
    ///   3. Create `run/airlock` and check again
    #[test]
    fn bridge_dir_plain_or_missing_is_accepted() {
        let root = temp_dir();
        let root = root.path();
        assert!(refuse_symlinks(root, BRIDGE_REL).is_ok());
        std::fs::create_dir_all(root.join("run")).unwrap();
        assert!(refuse_symlinks(root, BRIDGE_REL).is_ok());
        std::fs::create_dir_all(root.join(BRIDGE_REL)).unwrap();
        assert!(refuse_symlinks(root, BRIDGE_REL).is_ok());
    }

    /// Test that the symlink check refuses a bridge directory path with a
    /// symlink in it. The guest controls the rootfs, so a symlink could send
    /// the tmpfs mount to a path that the guest selects.
    ///   1. Make `run` or `run/airlock` a symlink to a directory, to the
    ///      parent or to a missing path
    ///   2. Check that each case is refused
    #[test]
    fn bridge_dir_with_symlinked_component_is_refused() {
        for (link, target) in [
            ("run", "elsewhere"),
            (BRIDGE_REL, "../"),
            (BRIDGE_REL, "/nowhere"),
        ] {
            let root = temp_dir();
            let root = root.path();
            std::fs::create_dir_all(root.join("elsewhere/airlock")).unwrap();
            if link == BRIDGE_REL {
                std::fs::create_dir_all(root.join("run")).unwrap();
            }
            std::os::unix::fs::symlink(target, root.join(link)).unwrap();
            assert!(
                refuse_symlinks(root, BRIDGE_REL).is_err(),
                "{link} -> {target}"
            );
        }
    }
}
