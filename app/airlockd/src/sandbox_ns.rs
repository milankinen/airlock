//! The shared sandbox mount namespace.
//!
//! Every container process (main process, `airlock exec`, daemons) enters the
//! assembled rootfs by joining this namespace with `setns`, not by `chroot`.
//!
//! **Why not `chroot`.** `chroot` only changes one process's root; the mount
//! namespace root stays the VM's initramfs, with every VM mount hanging off
//! it. Anything that later enters the namespace by `setns` — `docker exec`,
//! Docker healthchecks, `nsenter -m` — is put at the *namespace* root by the
//! kernel, and so lands in the VM instead of the container.
//!
//! **How the namespace is built.** A short-lived helper unshares a mount
//! namespace and `pivot_root`s into the rootfs, as OCI runtimes do, then
//! detaches the old root so no VM mount is left in the namespace. airlockd
//! keeps a descriptor to the namespace and the helper exits.
//!
//! `pivot_root` cannot be called from the VM root directly: that is the
//! initramfs `rootfs`, which has no parent mount, and `pivot_root` refuses to
//! move such a root (`EINVAL`). The helper first stacks a recursive bind of the
//! VM root on `/` and chroots into it; that bind has a parent, so it can be
//! pivoted away. The rootfs then takes its place on top of `/`. `setns`
//! resolves the namespace root with `LOOKUP_DOWN`, which follows mounts
//! stacked on `/`, so a process joining the namespace gets the container
//! rootfs as its root.
//!
//! Mounts made inside the sandbox after this point (e.g. dockerd bind-mounting
//! `/var/lib/docker`) stay in the sandbox namespace. VM-side paths under
//! [`ROOTFS`] keep working for airlockd, which stays in the VM namespace: new
//! *files* are shared through the same filesystems, only *mounts* are not.

use std::sync::OnceLock;

/// Where init assembles the container rootfs, as seen from the VM namespace.
pub const ROOTFS: &str = "/mnt/overlay/rootfs";

/// Empty VM directory the helper bind-mounts the VM root onto before it moves
/// that bind onto `/`. Only used inside the helper's namespace.
#[cfg(target_os = "linux")]
const STAGE: &str = "/mnt/.sandbox-stage";

/// Descriptor of the sandbox mount namespace (`O_CLOEXEC`), once created.
static NS_FD: OnceLock<i32> = OnceLock::new();

/// The sandbox namespace descriptor, or `None` if it was never created (init
/// failed to create it, or not on Linux). Spawns then fall back to `chroot`.
pub fn fd() -> Option<i32> {
    NS_FD.get().copied()
}

/// Create the sandbox namespace from the fully assembled rootfs.
///
/// Must run after every init mount under [`ROOTFS`] is in place: the
/// namespace gets a private copy of the mount tree at this moment, and later
/// VM-side mounts do not propagate into it.
#[cfg(target_os = "linux")]
pub fn create() -> anyhow::Result<()> {
    let fd = create_ns(ROOTFS)?;
    if NS_FD.set(fd).is_err() {
        unsafe { libc::close(fd) };
        anyhow::bail!("sandbox mount namespace already created");
    }
    Ok(())
}

/// Fork a helper that builds the namespace, grab `/proc/<pid>/ns/mnt`, then
/// let the helper exit. Returns the namespace descriptor.
#[cfg(target_os = "linux")]
fn create_ns(rootfs: &str) -> anyhow::Result<i32> {
    use anyhow::Context as _;

    std::fs::create_dir_all(STAGE).with_context(|| format!("create {STAGE}"))?;
    let rootfs_c = std::ffi::CString::new(rootfs)?;
    let stage_c = std::ffi::CString::new(STAGE)?;
    let dot = c".";
    let slash = c"/";

    // ready: helper → parent, a status byte (0 = ok, else the failed step)
    // followed by the step's errno.
    // release: parent → helper, closed once the descriptor is open.
    let ready = pipe().context("pipe")?;
    let release = match pipe() {
        Ok(p) => p,
        Err(e) => {
            close_pair(ready);
            return Err(e).context("pipe");
        }
    };

    // Safety: the child only makes async-signal-safe syscalls, then _exit.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        close_pair(ready);
        close_pair(release);
        return Err(std::io::Error::last_os_error()).context("fork");
    }
    if pid == 0 {
        unsafe {
            libc::close(ready.0);
            libc::close(release.1);
            let null = std::ptr::null::<libc::c_char>();
            let step: u8 = if libc::unshare(libc::CLONE_NEWNS) != 0 {
                1
            } else if libc::mount(
                null,
                slash.as_ptr(),
                null,
                libc::MS_REC | libc::MS_PRIVATE,
                std::ptr::null(),
            ) != 0
            {
                2
            // Stack a copy of the VM root (rootfs and all) on `/` and enter
            // it, so the root that pivot_root moves away has a parent.
            } else if libc::mount(
                slash.as_ptr(),
                stage_c.as_ptr(),
                null,
                libc::MS_BIND | libc::MS_REC,
                std::ptr::null(),
            ) != 0
            {
                3
            } else if libc::chdir(stage_c.as_ptr()) != 0
                || libc::mount(
                    dot.as_ptr(),
                    slash.as_ptr(),
                    null,
                    libc::MS_MOVE,
                    std::ptr::null(),
                ) != 0
            {
                4
            } else if libc::chroot(dot.as_ptr()) != 0 {
                5
            } else if libc::chdir(rootfs_c.as_ptr()) != 0 {
                6
            // pivot_root(".", ".") stacks the old root on top of the new one;
            // the umount detaches it, and every VM mount with it.
            } else if libc::syscall(libc::SYS_pivot_root, dot.as_ptr(), dot.as_ptr()) != 0 {
                7
            } else if libc::umount2(dot.as_ptr(), libc::MNT_DETACH) != 0 {
                8
            } else if libc::chdir(slash.as_ptr()) != 0 {
                9
            } else {
                0
            };
            let mut msg = [0u8; 5];
            msg[0] = step;
            if step != 0 {
                msg[1..].copy_from_slice(&(*libc::__errno_location()).to_ne_bytes());
            }
            libc::write(ready.1, msg.as_ptr().cast(), msg.len());
            // Block until the parent has opened the namespace (or died).
            let mut b = 0u8;
            libc::read(release.0, (&raw mut b).cast(), 1);
            libc::_exit(i32::from(step));
        }
    }

    unsafe {
        libc::close(ready.1);
        libc::close(release.0);
    }
    crate::process::register_own_child(pid as u32);

    let mut msg = [255u8; 5];
    let n = unsafe { libc::read(ready.0, msg.as_mut_ptr().cast(), msg.len()) };
    unsafe { libc::close(ready.0) };
    let step = if n == 5 { msg[0] } else { 255 };

    let result = if step == 0 {
        let path = std::ffi::CString::new(format!("/proc/{pid}/ns/mnt"))?;
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_CLOEXEC) };
        if fd < 0 {
            Err(std::io::Error::last_os_error()).context("open helper mount namespace")
        } else {
            Ok(fd)
        }
    } else {
        let what = match step {
            1 => "unshare(CLONE_NEWNS)",
            2 => "make / private",
            3 => "bind VM root onto stage",
            4 => "move stage onto /",
            5 => "chroot(stage)",
            6 => "chdir(rootfs)",
            7 => "pivot_root(rootfs)",
            8 => "detach old root",
            9 => "chdir(/)",
            _ => "helper died",
        };
        let err = match step {
            255 => anyhow::anyhow!("helper died"),
            _ => {
                std::io::Error::from_raw_os_error(i32::from_ne_bytes(msg[1..].try_into().unwrap()))
                    .into()
            }
        };
        Err(err.context(format!("sandbox mount namespace: {what} failed")))
    };

    // Release and reap the helper.
    unsafe {
        libc::close(release.1);
        libc::waitpid(pid, std::ptr::null_mut(), 0);
    }
    result
}

#[cfg(target_os = "linux")]
fn pipe() -> std::io::Result<(i32, i32)> {
    let mut fds = [-1i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

#[cfg(target_os = "linux")]
fn close_pair((r, w): (i32, i32)) {
    unsafe {
        libc::close(r);
        libc::close(w);
    }
}
