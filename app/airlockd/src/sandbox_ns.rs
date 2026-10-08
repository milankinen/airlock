//! The shared sandbox mount namespace.
//!
//! Gives all container processes (the main process, `airlock exec` and
//! daemons) one mount namespace with the container rootfs as its root. Tools
//! that enter a namespace from inside the sandbox, for example `docker exec`,
//! Docker health checks and `nsenter -m`, then also get the container and not
//! the VM.
//!
//! Mounts made inside the sandbox stay in the sandbox. airlockd itself stays
//! in the VM and can still see all new files in the rootfs.

use std::sync::OnceLock;

/// Path of the container rootfs in the VM mount namespace.
// airlockd stays in the VM namespace and uses this path. New files are shared
// with the sandbox namespace through the same filesystems. Only mounts are
// not shared.
pub const ROOTFS: &str = "/mnt/overlay/rootfs";

/// Empty VM directory. The helper bind-mounts the VM root here, then moves
/// that bind mount onto `/`. Used only inside the namespace of the helper.
#[cfg(target_os = "linux")]
const STAGE: &str = "/mnt/.sandbox-stage";

/// Descriptor of the sandbox mount namespace (`O_CLOEXEC`), once created.
static NS_FD: OnceLock<i32> = OnceLock::new();

/// Get the sandbox namespace descriptor.
///
/// Returns:
///   The descriptor, or `None` if init did not make the namespace (it failed,
///   or the OS is not Linux). Spawns then use `chroot`.
pub fn fd() -> Option<i32> {
    NS_FD.get().copied()
}

/// Make the sandbox namespace from the assembled rootfs.
///
/// Call it after all init mounts under [`ROOTFS`] are in place. The
/// namespace gets a private copy of the mount tree at this time. Later
/// mounts in the VM do not go into it.
///
/// Container processes join this namespace with `setns`, not `chroot`.
/// `chroot` changes the root of one process only. The root of its mount
/// namespace stays the VM initramfs, with all VM mounts under it. The kernel
/// puts a process that enters with `setns` at the namespace root, so it gets
/// the VM and not the container.
///
/// Returns:
///   Error if the namespace cannot be made, or if it already exists.
#[cfg(target_os = "linux")]
pub fn create() -> anyhow::Result<()> {
    let fd = create_ns(ROOTFS)?;
    if NS_FD.set(fd).is_err() {
        unsafe { libc::close(fd) };
        anyhow::bail!("sandbox mount namespace already created");
    }
    Ok(())
}

/// Make the namespace in a helper process and open it.
///
/// The helper unshares a mount namespace and moves its root into `rootfs`
/// with `pivot_root`, as OCI runtimes do. Then it detaches the old root, so
/// no VM mount stays in the namespace. The parent opens
/// `/proc/<pid>/ns/mnt`, and then the helper exits.
///
/// `pivot_root` cannot move the VM root directly. The VM root is the
/// initramfs `rootfs`, which has no parent mount, and `pivot_root` refuses
/// such a root (`EINVAL`). So the helper first puts a recursive bind of the
/// VM root on `/` and does a chroot into it. That bind has a parent, so
/// `pivot_root` can move it. The rootfs then sits on top of `/`. `setns`
/// finds the namespace root with `LOOKUP_DOWN`, which follows mounts on `/`.
/// Thus a process that joins the namespace gets the container rootfs as
/// its root.
///
/// Returns:
///   The namespace descriptor (`O_CLOEXEC`), or the error of the step that
///   failed.
#[cfg(target_os = "linux")]
fn create_ns(rootfs: &str) -> anyhow::Result<i32> {
    use anyhow::Context as _;

    std::fs::create_dir_all(STAGE).with_context(|| format!("create {STAGE}"))?;
    let rootfs_c = std::ffi::CString::new(rootfs)?;
    let stage_c = std::ffi::CString::new(STAGE)?;
    let dot = c".";
    let slash = c"/";

    // `ready`: helper to parent. A status byte (0 = ok, else the number of
    // the failed step), then the errno of that step.
    // `release`: parent to helper. The parent closes it when the descriptor
    // is open.
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
            // Put a copy of the VM root (with all its mounts) on `/` and
            // enter it. Thus the root that pivot_root moves has a parent.
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
            // pivot_root(".", ".") puts the old root on top of the new one.
            // The umount detaches it, and all VM mounts with it.
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
            // Wait until the parent opened the namespace (or stopped).
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

/// Make a pipe with `O_CLOEXEC` on both ends.
///
/// Returns:
///   The read and write descriptors.
#[cfg(target_os = "linux")]
fn pipe() -> std::io::Result<(i32, i32)> {
    let mut fds = [-1i32; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((fds[0], fds[1]))
}

/// Close both ends of a pipe.
#[cfg(target_os = "linux")]
fn close_pair((r, w): (i32, i32)) {
    unsafe {
        libc::close(r);
        libc::close(w);
    }
}
