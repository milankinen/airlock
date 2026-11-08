//! Landlock file system limits for host helper processes.
//!
//! Limits a helper process to a set of directories and files. All other
//! files of the host user stay out of its reach, also when the helper has a
//! bug that a guest can use. The limit starts before the helper program
//! runs and stays for the life of the process. The limit also stops new
//! TCP connections and signals to processes out of the limit, when the
//! kernel supports it.
//!
//! A kernel without Landlock, or with a Landlock version that is too old,
//! runs the helper without the limit.

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::process::CommandExt;
use std::process::Command;

use crate::util::VerifiedPath;

/// Access that a rule gives to a directory or a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Access {
    /// Read and run files, list directories.
    Read,
    /// All file system operations.
    ReadWrite,
}

// File system access rights of the kernel interface, by ABI version.
const FS_EXECUTE: u64 = 1 << 0;
const FS_WRITE_FILE: u64 = 1 << 1;
const FS_READ_FILE: u64 = 1 << 2;
const FS_READ_DIR: u64 = 1 << 3;
/// All rights of ABI 1, from bit 0 to bit 12.
const FS_ABI1: u64 = (1 << 13) - 1;
const FS_REFER: u64 = 1 << 13;
const FS_TRUNCATE: u64 = 1 << 14;
const FS_IOCTL_DEV: u64 = 1 << 15;
/// Rights that a rule for a file (not a directory) can have.
const FS_FILE: u64 = FS_EXECUTE | FS_WRITE_FILE | FS_READ_FILE | FS_TRUNCATE | FS_IOCTL_DEV;

const NET_BIND_TCP: u64 = 1 << 0;
const NET_CONNECT_TCP: u64 = 1 << 1;

const SCOPE_ABSTRACT_UNIX_SOCKET: u64 = 1 << 0;
const SCOPE_SIGNAL: u64 = 1 << 1;

const CREATE_RULESET_VERSION: u32 = 1 << 0;
const RULE_PATH_BENEATH: libc::c_int = 1;

/// `struct landlock_ruleset_attr` of the kernel interface.
#[repr(C)]
struct RulesetAttr {
    handled_access_fs: u64,
    handled_access_net: u64,
    scoped: u64,
}

/// `struct landlock_path_beneath_attr` of the kernel interface. The kernel
/// declares it packed.
#[repr(C, packed)]
struct PathBeneathAttr {
    allowed_access: u64,
    parent_fd: i32,
}

/// Get the Landlock ABI version of the kernel.
/// Returns:
///   The version, or `None` if the kernel does not have Landlock or it is
///   off.
pub(super) fn abi_version() -> Option<u32> {
    // SAFETY: a version query reads no memory.
    let version = unsafe {
        libc::syscall(
            libc::SYS_landlock_create_ruleset,
            std::ptr::null::<RulesetAttr>(),
            0usize,
            CREATE_RULESET_VERSION,
        )
    };
    u32::try_from(version).ok().filter(|&v| v > 0)
}

/// A set of Landlock rules that is not in use yet.
pub(super) struct Ruleset {
    fd: OwnedFd,
    /// All file system rights that the kernel knows. The ruleset blocks the
    /// rights that no rule gives.
    handled_fs: u64,
}

impl Ruleset {
    /// Make an empty ruleset.
    /// Returns:
    ///   The ruleset, `None` if the kernel has no usable Landlock, or error
    ///   if the kernel refuses the ruleset.
    pub(super) fn new() -> io::Result<Option<Self>> {
        // ABI 1 always refuses a rename or a link to a different directory.
        // The guest needs those operations, so ABI 1 is not usable.
        let Some(abi) = abi_version().filter(|&v| v >= 2) else {
            return Ok(None);
        };
        let mut handled_fs = FS_ABI1 | FS_REFER;
        if abi >= 3 {
            handled_fs |= FS_TRUNCATE;
        }
        if abi >= 5 {
            handled_fs |= FS_IOCTL_DEV;
        }
        let attr = RulesetAttr {
            handled_access_fs: handled_fs,
            // No rule gives a TCP right, so all TCP binds and connects fail.
            handled_access_net: NET_BIND_TCP | NET_CONNECT_TCP,
            scoped: SCOPE_ABSTRACT_UNIX_SOCKET | SCOPE_SIGNAL,
        };
        // An older kernel refuses fields that it does not know. Give it
        // only the part of the struct that it knows.
        let size = match abi {
            ..=3 => size_of::<u64>(),
            4..=5 => 2 * size_of::<u64>(),
            _ => size_of::<RulesetAttr>(),
        };
        // SAFETY: `attr` is valid for `size` bytes.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_landlock_create_ruleset,
                &raw const attr,
                size,
                0u32,
            )
        };
        let fd = libc::c_int::try_from(fd)
            .ok()
            .filter(|&fd| fd >= 0)
            .ok_or_else(io::Error::last_os_error)?;
        // SAFETY: the kernel gave a new fd (with close-on-exec).
        let fd = unsafe { OwnedFd::from_raw_fd(fd) };
        Ok(Some(Self { fd, handled_fs }))
    }

    /// Give `access` to `path`. For a directory, the access is also to all
    /// files and directories in it.
    /// Returns:
    ///   Error if `path` does not open or the kernel refuses the rule.
    pub(super) fn allow(&mut self, path: &VerifiedPath, access: Access) -> io::Result<()> {
        let file = path.open_with(
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_PATH | libc::O_CLOEXEC),
        )?;
        let mut allowed = match access {
            Access::Read => FS_EXECUTE | FS_READ_FILE | FS_READ_DIR,
            Access::ReadWrite => self.handled_fs,
        };
        // The kernel refuses directory rights in a rule for a file.
        if !file.metadata()?.is_dir() {
            allowed &= FS_FILE;
        }
        let attr = PathBeneathAttr {
            allowed_access: allowed & self.handled_fs,
            parent_fd: file.as_raw_fd(),
        };
        // SAFETY: `attr` is a valid rule and the fd is open.
        let res = unsafe {
            libc::syscall(
                libc::SYS_landlock_add_rule,
                self.fd.as_raw_fd(),
                RULE_PATH_BENEATH,
                &raw const attr,
                0u32,
            )
        };
        if res != 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }

    /// Give `access` to each path in `paths` that exists. Paths that do
    /// not exist are skipped.
    /// Returns:
    ///   Error if the kernel refuses a rule.
    pub(super) fn allow_if_exists(&mut self, paths: &[&str], access: Access) -> io::Result<()> {
        for path in paths {
            let Ok(path) = VerifiedPath::new_follow(path, None) else {
                continue;
            };
            match self.allow(&path, access) {
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                other => other?,
            }
        }
        Ok(())
    }

    /// Put the ruleset on the process of `cmd` before its program starts.
    pub(super) fn apply_on_exec(self, cmd: &mut Command) {
        let fd = self.fd;
        // SAFETY: only async-signal-safe calls (two syscalls) after fork.
        unsafe {
            cmd.pre_exec(move || {
                // Landlock needs this flag for a process without
                // `CAP_SYS_ADMIN`.
                if libc::prctl(libc::PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0) != 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::syscall(libc::SYS_landlock_restrict_self, fd.as_raw_fd(), 0u32) != 0 {
                    return Err(io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the Landlock limits of helper processes.

    use std::process::Command;

    use super::{Access, Ruleset};
    use crate::test_cfg::temp_dir;
    use crate::util::{Roots, VerifiedPath};

    /// Test that a helper process with a ruleset reads and writes only the
    /// given directory. A helper with a bug must not reach other files of
    /// the host user.
    ///   1. Make an allowed directory and a secret file out of it
    ///   2. Start a shell with the ruleset. It writes and reads a file in the
    ///      allowed directory, then reads the secret
    ///   3. Check that the allowed file operations worked and the read of
    ///      the secret failed
    #[test]
    fn helper_with_ruleset_reaches_only_allowed_dirs() {
        let Some(mut rules) = Ruleset::new().unwrap() else {
            eprintln!("skipped: no usable Landlock");
            return;
        };
        let tmp = temp_dir();
        let root = tmp.path();
        let allowed = root.join("allowed");
        std::fs::create_dir(&allowed).unwrap();
        let secret = root.join("secret");
        std::fs::write(&secret, b"SECRET").unwrap();
        let roots = Roots::resolve(root, root, root).unwrap();
        rules
            .allow(
                &VerifiedPath::new(&allowed, &roots).unwrap(),
                Access::ReadWrite,
            )
            .unwrap();
        rules
            .allow_if_exists(&["/bin", "/usr", "/lib", "/lib64", "/etc"], Access::Read)
            .unwrap();

        let mut cmd = Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(r#"echo ok > "$1/out" && cat "$1/out" > "$1/copy" && ! cat "$2""#)
            .arg("sh")
            .arg(&allowed)
            .arg(&secret);
        rules.apply_on_exec(&mut cmd);
        let status = cmd.status().unwrap();

        assert!(status.success(), "{status}");
        // The copy shows that the shell could run and read the allowed files.
        assert_eq!(std::fs::read(allowed.join("copy")).unwrap(), b"ok\n");
    }
}
