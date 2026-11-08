//! Mount helpers for guest init.
//!
//! Small helpers that mount host shares, bind mounts and other filesystems,
//! with clear error messages. Shares can map the host owner to a guest user.

use std::ffi::CString;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

use tracing::debug;

use crate::init::IdMap;

/// Mount a VirtioFS share by its tag name at `/mnt/<tag>`.
/// Args:
///  - `tag`: VirtioFS tag of the share
///  - `idmap`: Id map to put on the share
pub(super) fn virtiofs(tag: &str, idmap: Option<&IdMapNs>) -> anyhow::Result<()> {
    let mount_point = format!("/mnt/{tag}");
    std::fs::create_dir_all(&mount_point)?;
    virtiofs_at(tag, &mount_point, idmap).map_err(|(step, e)| {
        anyhow::anyhow!("mount virtiofs {tag} at {mount_point}: {step}: {e}")
    })?;
    debug!("mounted virtiofs: {tag} → {mount_point}");
    Ok(())
}

/// Make a detached VirtioFS mount, put the idmap on it and attach it at
/// `mount_point`. The kernel sets an idmap only on a detached mount, so the
/// new mount API is necessary.
/// Returns:
///   Error with the name of the failed step.
fn virtiofs_at(
    tag: &str,
    mount_point: &str,
    idmap: Option<&IdMapNs>,
) -> Result<(), (&'static str, std::io::Error)> {
    let fstype = CString::new("virtiofs").unwrap();
    let source = CString::new("source").unwrap();
    let tag = CString::new(tag).unwrap();
    let target = CString::new(mount_point).unwrap();
    let empty = CString::new("").unwrap();
    let fail = |step| (step, std::io::Error::last_os_error());

    // SAFETY (all syscalls below): the strings are null-terminated and the
    // fds are open. Each returned fd is checked.
    let fs = unsafe { libc::syscall(libc::SYS_fsopen, fstype.as_ptr(), libc::FSOPEN_CLOEXEC) };
    let fs = owned_fd(fs).ok_or_else(|| fail("fsopen"))?;
    let res = unsafe {
        libc::syscall(
            libc::SYS_fsconfig,
            fs.as_raw_fd(),
            libc::FSCONFIG_SET_STRING,
            source.as_ptr(),
            tag.as_ptr(),
            0,
        )
    };
    check(res).map_err(|()| fail("set source"))?;
    let res = unsafe {
        libc::syscall(
            libc::SYS_fsconfig,
            fs.as_raw_fd(),
            libc::FSCONFIG_CMD_CREATE,
            std::ptr::null::<libc::c_char>(),
            std::ptr::null::<libc::c_void>(),
            0,
        )
    };
    check(res).map_err(|()| fail("create"))?;
    let mnt = unsafe { libc::syscall(libc::SYS_fsmount, fs.as_raw_fd(), libc::FSMOUNT_CLOEXEC, 0) };
    let mnt = owned_fd(mnt).ok_or_else(|| fail("fsmount"))?;

    if let Some(idmap) = idmap {
        // FUSE allows the idmap only after its async init handshake. A
        // request waits for it.
        let mut st = std::mem::MaybeUninit::<libc::statfs>::uninit();
        let res = unsafe { libc::fstatfs(mnt.as_raw_fd(), st.as_mut_ptr()) };
        check(res.into()).map_err(|()| fail("statfs"))?;
        let attr = libc::mount_attr {
            attr_set: libc::MOUNT_ATTR_IDMAP,
            attr_clr: 0,
            propagation: 0,
            userns_fd: u64::try_from(idmap.0.as_raw_fd()).expect("fd is not negative"),
        };
        let res = unsafe {
            libc::syscall(
                libc::SYS_mount_setattr,
                mnt.as_raw_fd(),
                empty.as_ptr(),
                libc::AT_EMPTY_PATH,
                &raw const attr,
                size_of::<libc::mount_attr>(),
            )
        };
        check(res).map_err(|()| fail("set idmap"))?;
    }

    let res = unsafe {
        libc::syscall(
            libc::SYS_move_mount,
            mnt.as_raw_fd(),
            empty.as_ptr(),
            libc::AT_FDCWD,
            target.as_ptr(),
            libc::MOVE_MOUNT_F_EMPTY_PATH,
        )
    };
    check(res).map_err(|()| fail("attach"))
}

/// Check the result of a syscall that gives 0 on success.
fn check(res: libc::c_long) -> Result<(), ()> {
    if res == 0 { Ok(()) } else { Err(()) }
}

/// User namespace of an [`IdMap`], for idmapped mounts.
pub(super) struct IdMapNs(OwnedFd);

impl IdMapNs {
    /// Make the user namespace of `map`.
    /// Returns:
    ///   The namespace, `None` if the map changes no id, or error if the
    ///   kernel refuses it.
    pub(super) fn new(map: IdMap) -> anyhow::Result<Option<Self>> {
        if map.host_uid == map.guest_uid && map.host_gid == map.guest_gid {
            return Ok(None);
        }
        let (ready_r, ready_w) = std::io::pipe()?;
        let (done_r, done_w) = std::io::pipe()?;
        // SAFETY: the child calls only async-signal-safe syscalls.
        let pid = unsafe { libc::fork() };
        if pid < 0 {
            anyhow::bail!("fork for idmap: {}", std::io::Error::last_os_error());
        }
        if pid == 0 {
            // SAFETY: raw syscalls on own fds. The child lives until the
            // parent closes `done_w`.
            unsafe {
                let ok = libc::unshare(libc::CLONE_NEWUSER) == 0;
                let byte = u8::from(ok);
                libc::write(ready_w.as_raw_fd(), (&raw const byte).cast(), 1);
                libc::close(done_w.as_raw_fd());
                let mut buf = 0u8;
                libc::read(done_r.as_raw_fd(), (&raw mut buf).cast(), 1);
                libc::_exit(0);
            }
        }
        // Keep the orphan reaper away from this child.
        crate::process::register_own_child(pid.cast_unsigned());
        drop(ready_w);
        drop(done_r);
        let result = Self::open_ns(pid, ready_r, map);
        drop(done_w);
        loop {
            // SAFETY: `pid` is the child of this function.
            let res = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
            if res >= 0 || std::io::Error::last_os_error().kind() != std::io::ErrorKind::Interrupted
            {
                break;
            }
        }
        result.map(Some)
    }

    /// Write the maps of the child `pid` and open its user namespace.
    fn open_ns(
        pid: libc::pid_t,
        mut ready: std::io::PipeReader,
        map: IdMap,
    ) -> anyhow::Result<Self> {
        use std::io::Read;
        let mut byte = [0u8];
        ready.read_exact(&mut byte)?;
        if byte[0] != 1 {
            anyhow::bail!("idmap: kernel refused a new user namespace");
        }
        // The kernel needs each map in one write.
        std::fs::write(
            format!("/proc/{pid}/uid_map"),
            swap_map(map.host_uid, map.guest_uid),
        )?;
        std::fs::write(
            format!("/proc/{pid}/gid_map"),
            swap_map(map.host_gid, map.guest_gid),
        )?;
        let ns = std::fs::File::open(format!("/proc/{pid}/ns/user"))?;
        Ok(Self(ns.into()))
    }
}

/// Make an id map that swaps `a` with `b` and keeps all other ids. Lines
/// are `<file id> <guest id> <count>`, up to the last valid id 4294967294.
fn swap_map(a: u32, b: u32) -> String {
    use std::fmt::Write;
    const LAST: u32 = u32::MAX - 1;
    if a == b {
        return format!("0 0 {}\n", u64::from(LAST) + 1);
    }
    let (lo, hi) = (a.min(b), a.max(b));
    let mut map = format!("{a} {b} 1\n{b} {a} 1\n");
    for (first, last) in [
        (0, lo.checked_sub(1)),
        (lo + 1, hi.checked_sub(1)),
        (hi.saturating_add(1), Some(LAST)),
    ] {
        if let Some(last) = last
            && first <= last
        {
            let _ = writeln!(map, "{first} {first} {}", last - first + 1);
        }
    }
    map
}

/// Take the fd that a syscall returned, or `None` on failure.
fn owned_fd(ret: libc::c_long) -> Option<OwnedFd> {
    let fd = i32::try_from(ret).ok().filter(|fd| *fd >= 0)?;
    // SAFETY: the syscall gave a new fd that nothing else owns.
    Some(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// Bind-mount `src` at `dst`, optionally read-only.
pub(super) fn bind(src: &str, dst: &str, read_only: bool) -> anyhow::Result<()> {
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

/// Mount a filesystem.
/// Args:
///  - `source`: Mount source (device or name, for example `proc`)
///  - `target`: Mount point
///  - `fstype`: Filesystem type
///  - `flags`: `MS_*` mount flags
///  - `data`: Filesystem-specific options. Empty means no options.
pub(super) fn fs(
    source: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: &str,
) -> anyhow::Result<()> {
    let src_cstr = std::ffi::CString::new(source).unwrap();
    let dst_cstr = std::ffi::CString::new(target).unwrap();
    let fs_cstr = std::ffi::CString::new(fstype).unwrap();
    // Leak the CString to keep the pointer valid during the syscall.
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

#[cfg(test)]
mod tests {
    use super::swap_map;

    /// Test that the id map swaps two ids and keeps the others. The kernel
    /// refuses overlapping ranges.
    ///   1. Make the map for id pairs at the edges and in the middle
    ///   2. Check that both sides cover each id one time
    ///   3. Check that only the two ids change
    #[test]
    fn swap_map_swaps_two_ids_and_keeps_all_others() {
        const LAST: u64 = u32::MAX as u64 - 1;
        for (a, b) in [
            (501, 0),
            (0, 501),
            (501, 1000),
            (1, 0),
            (0, 1),
            (7, 7),
            (0, 4_294_967_294),
        ] {
            let lines: Vec<(u64, u64, u64)> = swap_map(a, b)
                .lines()
                .map(|l| {
                    let f: Vec<u64> = l.split(' ').map(|n| n.parse().unwrap()).collect();
                    (f[0], f[1], f[2])
                })
                .collect();
            for side in [0, 1] {
                let mut ranges: Vec<(u64, u64)> = lines
                    .iter()
                    .map(|l| (if side == 0 { l.0 } else { l.1 }, l.2))
                    .collect();
                ranges.sort_unstable();
                let mut next = 0;
                for (first, count) in ranges {
                    assert_eq!(first, next, "map {a}<->{b} has a gap or overlap");
                    next = first + count;
                }
                assert_eq!(next, LAST + 1, "map {a}<->{b} does not cover all ids");
            }
            let map = |id: u64| {
                let (inside, outside, _) = lines
                    .iter()
                    .find(|l| (l.0..l.0 + l.2).contains(&id))
                    .unwrap();
                outside + (id - inside)
            };
            assert_eq!(map(a.into()), u64::from(b));
            assert_eq!(map(b.into()), u64::from(a));
            for id in [0, 2, 500, 502, 999, 1001, LAST] {
                if id != u64::from(a) && id != u64::from(b) {
                    assert_eq!(map(id), id);
                }
            }
        }
    }
}
