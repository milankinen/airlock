//! Safe resolve of mount targets.
//!
//! Finds and makes the target directories of the mounts in the container
//! rootfs. Guest init mounts as VM root, so a symlink in a mount path
//! chooses where the mount goes. The resolve follows the symlinks of the
//! image, for example `/var/run -> /run`. It refuses a symlink that the
//! container user or another party can change:
//!  * a symlink in a directory that the container user can write
//!  * a symlink in a shared host directory
//!
//! A symlink can not go above the root. Each step holds the directory
//! open, so a change of the path during the resolve can not move the
//! result to a different place. New directories get the owner of their
//! parent directory.

use std::ffi::{CString, OsStr, OsString};
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

/// Symlink rules for the resolve.
pub(super) struct Policy {
    /// Container user ID.
    pub uid: u32,
    /// Container group ID.
    pub gid: u32,
    /// Device IDs of the shared host directories. A symlink in a directory
    /// on one of these devices is refused.
    pub shared_devs: Vec<u64>,
}

/// Error for a symlink that the policy refuses.
#[derive(Debug)]
pub(super) struct Refused(String);

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for Refused {}

/// A resolved directory.
pub(super) struct Resolved {
    /// `O_PATH` fd of the directory.
    pub fd: OwnedFd,
}

/// Open the directory `path` as an `O_PATH` fd. Use it for the root of a
/// resolve and for paths that only VM root can change.
pub(super) fn open_dir(path: &str) -> std::io::Result<OwnedFd> {
    let path = CString::new(path)?;
    // SAFETY: the string is null-terminated. The result is checked.
    let fd = unsafe {
        libc::open(
            path.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
        )
    };
    owned(fd)
}

/// Make the directory `path` below `root` and its missing parents. Opens
/// the directories that exist.
/// Args:
///  - `root`: Directory that is `/` for the path and its symlinks
///  - `path`: Path below `root`. Absolute and relative paths are the same.
///  - `policy`: Symlink rules
///
/// Returns:
///   The directory, a [`Refused`] error if the path has a refused symlink
///   or a file, or other error.
pub(super) fn mkdirs(
    root: BorrowedFd<'_>,
    path: &str,
    policy: &Policy,
) -> anyhow::Result<Resolved> {
    // Open directories from the root to the current name. `..` goes one
    // step back, and never above the root.
    let mut stack = vec![root.try_clone_to_owned()?];
    let mut todo: Vec<OsString> = Vec::new();
    crate::util::push_components(&mut todo, Path::new(path));
    let mut hops = 0;
    while let Some(name) = todo.pop() {
        if name == ".." {
            if stack.len() > 1 {
                stack.pop();
            }
            continue;
        }
        let parent = stack.last().expect("stack has the root");
        let err = match open_child(parent, &name) {
            Ok(fd) => {
                stack.push(fd);
                continue;
            }
            Err(e) => e,
        };
        match err.raw_os_error() {
            Some(libc::ENOENT) => {
                if let Some(fd) =
                    make_child(parent, &name).map_err(|e| anyhow::anyhow!("{path}: {e:#}"))?
                {
                    stack.push(fd);
                } else {
                    // An other process made the entry first. Check it again.
                    todo.push(name);
                }
            }
            // `O_NOFOLLOW` gives ELOOP for a symlink. `O_DIRECTORY` gives
            // ENOTDIR for a file, and for a symlink with `O_PATH`.
            Some(libc::ELOOP | libc::ENOTDIR) => {
                let st = stat_at(parent, &name, libc::AT_SYMLINK_NOFOLLOW)?;
                // A file in the path is not an error of the sandbox. The
                // container user or a share can put it there.
                if st.st_mode & libc::S_IFMT != libc::S_IFLNK {
                    return Err(
                        Refused(format!("{path}: {} is not a directory", name.display())).into(),
                    );
                }
                if let Some(why) = check_link(parent, policy)? {
                    return Err(
                        Refused(format!("{path}: symlink {} is in {why}", name.display())).into(),
                    );
                }
                hops += 1;
                if hops > 40 {
                    anyhow::bail!("{path}: too many symlinks");
                }
                let target = read_link_at(parent, &name)?;
                if target.as_bytes().first() == Some(&b'/') {
                    stack.truncate(1);
                }
                crate::util::push_components(&mut todo, Path::new(&target));
            }
            _ => return Err(anyhow::anyhow!("{path}: open {}: {err}", name.display())),
        }
    }
    Ok(Resolved {
        fd: stack.pop().expect("stack has the root"),
    })
}

/// Check if the policy lets the resolve follow a symlink in `parent`.
/// Returns:
///   The reason of the refusal as text if the symlink is refused, `None` if
///   the policy lets the resolve follow it, or error if `parent` can not be
///   read.
fn check_link(parent: &OwnedFd, policy: &Policy) -> std::io::Result<Option<&'static str>> {
    let st = stat_at(parent, OsStr::new(""), libc::AT_EMPTY_PATH)?;
    if policy.shared_devs.contains(&st.st_dev) {
        return Ok(Some("a shared host directory"));
    }
    // Container root can already change all files. A symlink gives it
    // nothing more.
    if policy.uid != 0 && writable(&st, policy.uid, policy.gid) {
        return Ok(Some("a directory that the container user can write"));
    }
    Ok(None)
}

/// Check if the user `uid`/`gid` can write the directory `st`, now or after
/// a `chmod`. The sticky bit gives no exception: the user can still add new
/// symlinks. Container processes have no supplementary groups, so only
/// `gid` is checked.
fn writable(st: &libc::stat, uid: u32, gid: u32) -> bool {
    // The owner can change the mode of the directory.
    st.st_uid == uid
        || (st.st_gid == gid && st.st_mode & libc::S_IWGRP != 0)
        || st.st_mode & libc::S_IWOTH != 0
}

/// Open the directory `name` in `parent`. Refuses a symlink at `name`.
fn open_child(parent: &OwnedFd, name: &OsStr) -> std::io::Result<OwnedFd> {
    let name = CString::new(name.as_bytes())?;
    // SAFETY: the fd is open and the string is null-terminated. The result
    // is checked.
    let fd = unsafe {
        libc::openat(
            parent.as_raw_fd(),
            name.as_ptr(),
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    owned(fd)
}

/// Make the directory `name` in `parent` and give it the owner of
/// `parent`.
/// Returns:
///   The new directory, `None` if `name` exists already, or error.
fn make_child(parent: &OwnedFd, name: &OsStr) -> anyhow::Result<Option<OwnedFd>> {
    let cname = CString::new(name.as_bytes())?;
    // SAFETY: the fd is open and the string is null-terminated.
    let ret = unsafe { libc::mkdirat(parent.as_raw_fd(), cname.as_ptr(), 0o755) };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        if err.raw_os_error() == Some(libc::EEXIST) {
            return Ok(None);
        }
        anyhow::bail!("mkdir {}: {err}", name.display());
    }
    // An other party with write access to the parent can replace the new
    // directory with a symlink. `O_NOFOLLOW` then refuses it.
    let fd = open_child(parent, name)
        .map_err(|e| Refused(format!("new dir {} changed: {e}", name.display())))?;
    let parent_st = stat_at(parent, OsStr::new(""), libc::AT_EMPTY_PATH)?;
    let st = stat_at(&fd, OsStr::new(""), libc::AT_EMPTY_PATH)?;
    // A share can refuse a chown, also to the same owner. The directory is
    // usable without it.
    if (st.st_uid, st.st_gid) != (parent_st.st_uid, parent_st.st_gid)
        && let Err(e) = chown(&fd, parent_st.st_uid, parent_st.st_gid)
    {
        tracing::warn!("new dir {}: {e:#}", name.display());
    }
    Ok(Some(fd))
}

/// Change the owner of the directory `fd`. Does not follow symlinks.
pub(super) fn chown(fd: &OwnedFd, uid: u32, gid: u32) -> anyhow::Result<()> {
    // SAFETY: the fd is open and the string is null-terminated.
    let ret = unsafe {
        libc::fchownat(
            fd.as_raw_fd(),
            c"".as_ptr(),
            uid,
            gid,
            libc::AT_EMPTY_PATH | libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if ret != 0 {
        anyhow::bail!("chown to {uid}:{gid}: {}", std::io::Error::last_os_error());
    }
    Ok(())
}

/// Get the status of `name` in `dir`, or of `dir` with an empty name and
/// `AT_EMPTY_PATH`.
pub(super) fn stat_at(
    dir: &OwnedFd,
    name: &OsStr,
    flags: libc::c_int,
) -> std::io::Result<libc::stat> {
    let name = CString::new(name.as_bytes())?;
    let mut st = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: the fd is open, the string is null-terminated and `st` has
    // space for the result.
    let ret = unsafe { libc::fstatat(dir.as_raw_fd(), name.as_ptr(), st.as_mut_ptr(), flags) };
    if ret != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: `fstatat` filled `st`.
    Ok(unsafe { st.assume_init() })
}

/// Read the target of the symlink `name` in `dir`.
fn read_link_at(dir: &OwnedFd, name: &OsStr) -> std::io::Result<OsString> {
    let name = CString::new(name.as_bytes())?;
    let mut buf = vec![0u8; libc::PATH_MAX as usize];
    // SAFETY: the fd is open, the string is null-terminated and `buf` has
    // `buf.len()` bytes.
    let len = unsafe {
        libc::readlinkat(
            dir.as_raw_fd(),
            name.as_ptr(),
            buf.as_mut_ptr().cast(),
            buf.len(),
        )
    };
    let len = usize::try_from(len).map_err(|_| std::io::Error::last_os_error())?;
    buf.truncate(len);
    Ok(OsStr::from_bytes(&buf).to_owned())
}

/// Take the fd that a libc call returned.
fn owned(fd: libc::c_int) -> std::io::Result<OwnedFd> {
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    // SAFETY: the call gave a new fd that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

#[cfg(test)]
mod tests {
    use std::os::fd::{AsFd, OwnedFd};
    use std::os::unix::fs::{MetadataExt, PermissionsExt, symlink};
    use std::path::Path;

    use airlock_test_utils::temp_dir;

    use super::{OsStr, Policy, Refused, mkdirs, open_dir, stat_at};

    /// Policy of a non-root container user with no shares.
    fn user_policy() -> Policy {
        Policy {
            uid: 1000,
            gid: 1000,
            shared_devs: Vec::new(),
        }
    }

    /// Check that `fd` is the directory at `path`.
    fn assert_is(fd: &OwnedFd, path: &Path) {
        let st = stat_at(fd, OsStr::new(""), libc::AT_EMPTY_PATH).unwrap();
        let meta = std::fs::symlink_metadata(path).unwrap();
        assert!(meta.is_dir(), "{} is not a dir", path.display());
        assert_eq!((st.st_dev, st.st_ino), (meta.dev(), meta.ino()));
    }

    /// Resolve `path` below `root` with `policy`.
    fn resolve(root: &Path, path: &str, policy: &Policy) -> anyhow::Result<OwnedFd> {
        let root = open_dir(root.to_str().unwrap()).unwrap();
        mkdirs(root.as_fd(), path, policy).map(|r| r.fd)
    }

    /// Test that the symlinks of the image are followed, because images
    /// need them, for example `/var/run -> /run` and merged `/usr`.
    ///   1. Make `var/run -> /run` and `lib -> usr/lib` in root-owned dirs
    ///   2. Resolve paths through both links
    ///   3. Check that the result and the new dirs are in the link targets
    #[test]
    fn image_link_in_root_owned_dir_is_followed() {
        let tmp = temp_dir();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("var")).unwrap();
        std::fs::create_dir_all(root.join("run")).unwrap();
        std::fs::create_dir_all(root.join("usr/lib")).unwrap();
        symlink("/run", root.join("var/run")).unwrap();
        symlink("usr/lib", root.join("lib")).unwrap();

        let fd = resolve(root, "/var/run/x", &user_policy()).unwrap();
        assert_is(&fd, &root.join("run/x"));
        let fd = resolve(root, "lib/y", &user_policy()).unwrap();
        assert_is(&fd, &root.join("usr/lib/y"));
    }

    /// Test that a symlink can not go above the root. The resolve runs as
    /// VM root, so a path outside of the rootfs is a VM path.
    ///   1. Make `a/x -> ../../../../etc` and `b -> /../..`
    ///   2. Check that both resolve to dirs below the root
    #[test]
    fn link_above_root_is_clamped_to_root() {
        let tmp = temp_dir();
        let root = tmp.path().join("root");
        std::fs::create_dir_all(root.join("a")).unwrap();
        symlink("../../../../etc", root.join("a/x")).unwrap();
        symlink("/../..", root.join("b")).unwrap();

        let fd = resolve(&root, "/a/x/new", &user_policy()).unwrap();
        assert_is(&fd, &root.join("etc/new"));
        let fd = resolve(&root, "/b/c", &user_policy()).unwrap();
        assert_is(&fd, &root.join("c"));
    }

    /// Test that a symlink in a dir that the container user can write is
    /// refused. Otherwise the user can point a root mount at any dir of
    /// the rootfs at the next boot.
    ///   1. Make a world-writable dir with a link to `/etc`
    ///   2. Check that the resolve through the link is refused
    ///   3. Check that no dir was made in the link target
    ///   4. Check that the same link is followed for a root container user
    #[test]
    fn link_in_user_writable_dir_is_refused() {
        let tmp = temp_dir();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::set_permissions(root.join("home"), std::fs::Permissions::from_mode(0o777))
            .unwrap();
        symlink("/etc", root.join("home/cache")).unwrap();

        let err = resolve(root, "/home/cache/x", &user_policy())
            .err()
            .unwrap();
        assert!(err.is::<Refused>(), "{err:#}");
        assert!(!root.join("etc/x").exists());

        let root_policy = Policy {
            uid: 0,
            gid: 0,
            shared_devs: Vec::new(),
        };
        let fd = resolve(root, "/home/cache/x", &root_policy).unwrap();
        assert_is(&fd, &root.join("etc/x"));
    }

    /// Test that a symlink in a dir of the container user is refused, also
    /// when the dir has no write bit. The owner can set the bit again and
    /// add a link. Needs root for the chown.
    ///   1. Make a dir owned by 1000 with mode 0555 and a link in it
    ///   2. Check that the resolve through the link is refused
    #[test]
    fn link_in_read_only_dir_of_user_is_refused() {
        // SAFETY: no arguments, no side effects.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skip: needs root");
            return;
        }
        let tmp = temp_dir();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("home/u")).unwrap();
        symlink("/etc", root.join("home/u/cache")).unwrap();
        std::os::unix::fs::chown(root.join("home/u"), Some(1000), Some(1000)).unwrap();
        std::fs::set_permissions(root.join("home/u"), std::fs::Permissions::from_mode(0o555))
            .unwrap();

        let err = resolve(root, "/home/u/cache/x", &user_policy())
            .err()
            .unwrap();
        assert!(err.is::<Refused>(), "{err:#}");
    }

    /// Test that a file in the path is refused and not a hard error. The
    /// container user or a share can put a file there, and a hard error
    /// stops the boot.
    ///   1. Make a file `a`
    ///   2. Check that the resolve of `a/b` is refused
    #[test]
    fn file_in_path_is_refused() {
        let tmp = temp_dir();
        std::fs::write(tmp.path().join("a"), "x").unwrap();

        let err = resolve(tmp.path(), "/a/b", &user_policy()).err().unwrap();
        assert!(err.is::<Refused>(), "{err:#}");
    }

    /// Test that a symlink loop stops with an error.
    ///   1. Make `a -> b` and `b -> a`
    ///   2. Check that the resolve of `a` gives an error
    #[test]
    fn symlink_loop_is_error() {
        let tmp = temp_dir();
        symlink("b", tmp.path().join("a")).unwrap();
        symlink("a", tmp.path().join("b")).unwrap();

        let err = resolve(tmp.path(), "/a", &user_policy()).err().unwrap();
        assert!(err.to_string().contains("too many symlinks"), "{err:#}");
    }

    /// Test that a symlink in a shared host dir is refused. Another sandbox
    /// or the host can change the link.
    ///   1. Make a link in a root-owned dir
    ///   2. Mark the device of the dir as shared
    ///   3. Check that the resolve through the link is refused
    #[test]
    fn link_in_shared_dir_is_refused() {
        let tmp = temp_dir();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("usr/local/bin")).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        symlink("/usr/local/bin", root.join("project/cache")).unwrap();
        let policy = Policy {
            shared_devs: vec![std::fs::metadata(root).unwrap().dev()],
            ..user_policy()
        };

        let err = resolve(root, "/project/cache", &policy).err().unwrap();
        assert!(err.is::<Refused>(), "{err:#}");
    }

    /// Test that new dirs get the owner of their parent dir. A cache in
    /// `~/.cargo/registry` then gives the user a writable `~/.cargo`, and
    /// a new dir in `/opt` stays root-owned. Needs root for the chown.
    ///   1. Make `home/u` owned by 1000:1000
    ///   2. Resolve `home/u/.cargo/registry` and `opt/foo`
    ///   3. Check the owners of the new dirs
    #[test]
    fn missing_target_parents_get_owner_of_existing_parent() {
        // SAFETY: no arguments, no side effects.
        if unsafe { libc::geteuid() } != 0 {
            eprintln!("skip: needs root");
            return;
        }
        let tmp = temp_dir();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("home/u")).unwrap();
        std::os::unix::fs::chown(root.join("home/u"), Some(1000), Some(1000)).unwrap();

        resolve(root, "/home/u/.cargo/registry", &user_policy()).unwrap();
        resolve(root, "/opt/foo", &user_policy()).unwrap();

        let owner = |p: &str| {
            let m = std::fs::metadata(root.join(p)).unwrap();
            (m.uid(), m.gid())
        };
        assert_eq!(owner("home/u/.cargo"), (1000, 1000));
        assert_eq!(owner("home/u/.cargo/registry"), (1000, 1000));
        assert_eq!(owner("opt/foo"), (0, 0));
    }
}
