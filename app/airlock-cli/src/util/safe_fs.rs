//! Symlink-safe host file access.
//!
//! Some host directories are mounted read-write into the guest, for example
//! the mounts of the agent packs. The guest can put symlinks in them. Host
//! code that reads or writes files there must not follow such a link to a
//! path that the guest selects. This module gives file access that prevents
//! this.

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// An open directory. All file operations resolve relative to it and never
/// follow a symlink at the final component. A later swap of a directory on
/// the path cannot redirect them.
///
/// Files that are read or appended to must be regular files that the
/// current user owns. Atomic writes and copies replace the entry at the
/// name.
pub struct PinnedDir {
    // Each operation is an `*at()` syscall relative to this descriptor with
    // `O_NOFOLLOW`. Thus a symlink at the file name cannot redirect it, and
    // a swap of a directory on the path cannot redirect it either. Atomic
    // writes and copies use an `O_EXCL` temp file and `renameat`.
    fd: OwnedFd,
    /// The path of the directory when it was opened. Only for messages.
    /// Never give it to a syscall.
    display: PathBuf,
}

impl PinnedDir {
    /// Pin the directory at `path`. Only the last component must not be a
    /// symlink. The components before it resolve normally.
    pub fn pin(path: &Path) -> io::Result<Self> {
        let c = CString::new(path.as_os_str().as_bytes())?;
        let fd = open_raw(
            libc::AT_FDCWD,
            &c,
            libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW,
            0,
        )?;
        Ok(Self {
            fd,
            display: path.to_path_buf(),
        })
    }

    /// Open the directory `root/rel`.
    /// Args:
    ///  - `root`: Trusted base directory. It resolves normally and can
    ///    contain symlinks (for example `/tmp` on macOS).
    ///  - `rel`: Plain relative path. Each component opens with
    ///    `O_NOFOLLOW` and must be a directory that the current user owns.
    ///  - `create`: If `true`, create missing components with mode 0700.
    ///
    /// Returns:
    ///   The pinned directory. Error `NotFound` if a component is missing
    ///   and `create` is `false`.
    pub fn open(root: &Path, rel: &Path, create: bool) -> io::Result<Self> {
        let mut dir = Self::pin(&std::fs::canonicalize(root)?)?;
        for component in rel.components() {
            let Component::Normal(name) = component else {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("not a plain relative path: {}", rel.display()),
                ));
            };
            dir = dir.subdir(name, create)?;
        }
        Ok(dir)
    }

    /// Open the child directory `name`. If `create` is `true` and the
    /// directory does not exist, create it with mode 0700.
    fn subdir(&self, name: &OsStr, create: bool) -> io::Result<Self> {
        let c = file_name(name)?;
        let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW;
        let fd = match open_raw(self.fd.as_raw_fd(), &c, flags, 0) {
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                let rc = unsafe { libc::mkdirat(self.fd.as_raw_fd(), c.as_ptr(), 0o700) };
                if rc != 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() != io::ErrorKind::AlreadyExists {
                        return Err(err);
                    }
                }
                open_raw(self.fd.as_raw_fd(), &c, flags, 0)?
            }
            other => other?,
        };
        let file = File::from(fd);
        check_owner(&file.metadata()?, &self.display.join(name))?;
        Ok(Self {
            fd: file.into(),
            display: self.display.join(name),
        })
    }

    /// Get the path of the directory when it was opened, for messages.
    pub fn path(&self) -> &Path {
        &self.display
    }

    /// Read the regular file `name`.
    /// Args:
    ///  - `name`: File name
    ///  - `cap`: Maximum file size in bytes
    ///
    /// Returns:
    ///   The file content, or `Ok(None)` if the file does not exist. Error
    ///   for a symlink, a file that is not regular, a file of a different
    ///   owner, or a file larger than `cap`.
    pub fn read(&self, name: impl AsRef<OsStr>, cap: u64) -> io::Result<Option<Vec<u8>>> {
        let name = name.as_ref();
        let c = file_name(name)?;
        // O_NONBLOCK: the open of a planted FIFO must not block. The type
        // check below refuses it.
        let fd = match open_raw(
            self.fd.as_raw_fd(),
            &c,
            libc::O_RDONLY | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            0,
        ) {
            Ok(fd) => fd,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(self.context(name, &e)),
        };
        let file = File::from(fd);
        let meta = file.metadata()?;
        self.check_regular(name, &meta)?;
        if meta.len() > cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is larger than {cap} bytes",
                    self.display.join(name).display()
                ),
            ));
        }
        let mut buf = Vec::with_capacity(meta.len() as usize);
        file.take(cap + 1).read_to_end(&mut buf)?;
        if buf.len() as u64 > cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "{} is larger than {cap} bytes",
                    self.display.join(name).display()
                ),
            ));
        }
        Ok(Some(buf))
    }

    /// Replace the file `name` atomically with `bytes`. The file gets the
    /// permission bits `mode`. A symlink at `name` is replaced, not followed.
    pub fn write_atomic(&self, name: impl AsRef<OsStr>, bytes: &[u8], mode: u32) -> io::Result<()> {
        let name = name.as_ref();
        // Write a new `O_EXCL` temp file, sync it, and `renameat` it over
        // `name`.
        self.replace_with(name, mode, |out| out.write_all(bytes))
    }

    /// Open the file `name` to append to it. If the file does not exist,
    /// create it with the permission bits `mode`.
    pub fn open_append(&self, name: impl AsRef<OsStr>, mode: u32) -> io::Result<File> {
        let name = name.as_ref();
        let c = file_name(name)?;
        let fd = open_raw(
            self.fd.as_raw_fd(),
            &c,
            libc::O_WRONLY | libc::O_APPEND | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK,
            mode,
        )
        .map_err(|e| self.context(name, &e))?;
        let file = File::from(fd);
        self.check_regular(name, &file.metadata()?)?;
        Ok(file)
    }

    /// Remove `name` (a file or a symlink, never followed).
    /// Returns:
    ///   `Ok(true)` if it was removed, `Ok(false)` if it did not exist.
    pub fn remove(&self, name: impl AsRef<OsStr>) -> io::Result<bool> {
        let c = file_name(name.as_ref())?;
        let rc = unsafe { libc::unlinkat(self.fd.as_raw_fd(), c.as_ptr(), 0) };
        if rc != 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::NotFound {
                return Ok(false);
            }
            return Err(err);
        }
        Ok(true)
    }

    /// Get the inode of `name`. Does not follow a symlink.
    /// Returns:
    ///   The inode, or `None` if `name` does not exist or `fstatat` fails.
    pub fn ino(&self, name: impl AsRef<OsStr>) -> Option<u64> {
        let c = file_name(name.as_ref()).ok()?;
        let mut st: libc::stat = unsafe { std::mem::zeroed() };
        let rc = unsafe {
            libc::fstatat(
                self.fd.as_raw_fd(),
                c.as_ptr(),
                &raw mut st,
                libc::AT_SYMLINK_NOFOLLOW,
            )
        };
        (rc == 0).then_some(st.st_ino as u64)
    }

    /// Make `name` a hard link to the open file `src`, atomically. Only on
    /// Linux. On other platforms, the result is an `Unsupported` error.
    #[cfg(target_os = "linux")]
    pub fn link_from(&self, src: &File, name: impl AsRef<OsStr>) -> io::Result<()> {
        let name = name.as_ref();
        // Link to a temp name, then rename. The link goes through
        // `/proc/self/fd`, so it targets exactly the inode that `src` has open.
        let tmp = temp_name(name);
        let src_path = CString::new(format!("/proc/self/fd/{}", src.as_raw_fd()))?;
        let rc = unsafe {
            libc::linkat(
                libc::AT_FDCWD,
                src_path.as_ptr(),
                self.fd.as_raw_fd(),
                tmp.as_ptr(),
                libc::AT_SYMLINK_FOLLOW,
            )
        };
        if rc != 0 {
            return Err(io::Error::last_os_error());
        }
        self.commit_temp(&tmp, name)
    }

    /// Make `name` a hard link to the open file `src`. Not available on this
    /// platform: always returns an `Unsupported` error. macOS has no
    /// `/proc/self/fd` and no `linkat(AT_EMPTY_PATH)`, so a hard link cannot
    /// be made from an open file.
    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unused_self)] // Same signature as on Linux.
    pub fn link_from(&self, _src: &File, _name: impl AsRef<OsStr>) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "linkat from fd not available on this platform",
        ))
    }

    /// Replace the file `name` atomically with the current content of the
    /// open file `src`. The copy reads from the descriptor, not from a path.
    /// The file gets the permission bits `mode`.
    pub fn copy_from(&self, src: &File, name: impl AsRef<OsStr>, mode: u32) -> io::Result<()> {
        let mut src = src;
        self.replace_with(name.as_ref(), mode, |out| {
            io::copy(&mut src, out).map(|_| ())
        })
    }

    /// Write a temp file with `fill`, sync it and rename it over `name`. If a
    /// step fails, the temp file is removed.
    fn replace_with(
        &self,
        name: &OsStr,
        mode: u32,
        fill: impl FnOnce(&mut File) -> io::Result<()>,
    ) -> io::Result<()> {
        file_name(name)?;
        let tmp = temp_name(name);
        let fd = open_raw(
            self.fd.as_raw_fd(),
            &tmp,
            libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW,
            mode,
        )?;
        let mut out = File::from(fd);
        let written = fill(&mut out).and_then(|()| out.sync_all());
        drop(out);
        match written {
            Ok(()) => self.commit_temp(&tmp, name),
            Err(e) => {
                unsafe { libc::unlinkat(self.fd.as_raw_fd(), tmp.as_ptr(), 0) };
                Err(e)
            }
        }
    }

    /// Rename the temp file `tmp` over `name`. If the rename fails, remove
    /// the temp file.
    fn commit_temp(&self, tmp: &CString, name: &OsStr) -> io::Result<()> {
        let target = file_name(name)?;
        let rc = unsafe {
            libc::renameat(
                self.fd.as_raw_fd(),
                tmp.as_ptr(),
                self.fd.as_raw_fd(),
                target.as_ptr(),
            )
        };
        if rc != 0 {
            let err = io::Error::last_os_error();
            unsafe { libc::unlinkat(self.fd.as_raw_fd(), tmp.as_ptr(), 0) };
            return Err(err);
        }
        Ok(())
    }

    /// Make sure that `name` is a regular file that the current user owns.
    fn check_regular(&self, name: &OsStr, meta: &std::fs::Metadata) -> io::Result<()> {
        let path = self.display.join(name);
        if !meta.file_type().is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a regular file", path.display()),
            ));
        }
        check_owner(meta, &path)
    }

    /// Add the path of `name` to the error `e`. A symlink error (`ELOOP`)
    /// gets a clear message.
    fn context(&self, name: &OsStr, e: &io::Error) -> io::Error {
        let path = self.display.join(name);
        if e.raw_os_error() == Some(libc::ELOOP) {
            return io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is a symlink; refusing to follow it", path.display()),
            );
        }
        io::Error::new(e.kind(), format!("{}: {e}", path.display()))
    }
}

/// Call `openat` with `O_CLOEXEC`.
/// Returns:
///   The new owned descriptor.
fn open_raw(
    dirfd: libc::c_int,
    name: &CString,
    flags: libc::c_int,
    mode: u32,
) -> io::Result<OwnedFd> {
    let raw = unsafe {
        libc::openat(
            dirfd,
            name.as_ptr(),
            flags | libc::O_CLOEXEC,
            mode as libc::c_uint,
        )
    };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: openat returned a fresh descriptor that nothing else owns.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

/// Convert one path component to a C string.
/// Returns:
///   The C string, or error if `name` is empty, `.`, `..` or contains `/`.
fn file_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes == b"." || bytes == b".." || bytes.contains(&b'/') {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("not a file name: {}", name.to_string_lossy()),
        ));
    }
    Ok(CString::new(bytes)?)
}

/// Make a temp file name for `name`, unique in the process.
fn temp_name(name: &OsStr) -> CString {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut bytes = b".".to_vec();
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(format!(".{}.{n}.airlock-tmp", std::process::id()).as_bytes());
    CString::new(bytes).expect("file names have no NUL bytes")
}

/// Make sure that the user who runs airlock owns the file or directory.
fn check_owner(meta: &std::fs::Metadata, path: &Path) -> io::Result<()> {
    let euid = unsafe { libc::geteuid() };
    if meta.uid() != euid {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "{} is owned by uid {}, not by the current user",
                path.display(),
                meta.uid()
            ),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Tests for the pinned directory: private files, size limits and
    //! symlink safety.

    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::test_cfg::temp_dir;

    /// The permission bits of `path`.
    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// Test that a pinned directory makes private directories and files,
    /// writes atomically and refuses reads over the size limit.
    ///   1. Open a nested directory with create and check its path and mode
    ///   2. Check that a missing directory without create fails
    ///   3. Write a file two times and check its mode and that no temporary
    ///      file is left
    ///   4. Read the file at and over the size limit, and read a missing file
    ///   5. Remove the file two times and check the results
    #[test]
    fn pinned_dir_stores_files_owner_only_within_size_cap() {
        let tmp = temp_dir();
        let dir = PinnedDir::open(tmp.path(), Path::new("a/b"), true).unwrap();
        assert_eq!(
            dir.path(),
            std::fs::canonicalize(tmp.path()).unwrap().join("a/b")
        );
        assert_eq!(mode(&tmp.path().join("a/b")), 0o700);
        assert!(PinnedDir::open(tmp.path(), Path::new("missing"), false).is_err());

        dir.write_atomic("state.json", b"1", 0o600).unwrap();
        dir.write_atomic("state.json", &[b'a'; 10], 0o600).unwrap();
        assert_eq!(mode(&tmp.path().join("a/b/state.json")), 0o600);
        let names: Vec<_> = std::fs::read_dir(tmp.path().join("a/b"))
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["state.json"]);
        assert_eq!(dir.read("state.json", 10).unwrap().unwrap().len(), 10);
        assert!(dir.read("state.json", 9).is_err());
        assert!(dir.read("missing", 9).unwrap().is_none());

        assert!(dir.remove("state.json").unwrap());
        assert!(!dir.remove("state.json").unwrap());
    }

    /// Test that a pinned directory never follows a symlink or `..` out of
    /// itself, so that a guest-controlled path cannot reach host files.
    ///   1. Open a directory through a symlinked path part and check the
    ///      error, then open a `..` path and check the error
    ///   2. Put a symlink to a host file in the directory
    ///   3. Check that a read and an append through the symlink fail, and
    ///      that a `..` read fails
    ///   4. Write the file name and check that the write replaces the
    ///      symlink and the host file did not change
    #[test]
    fn pinned_dir_never_leaves_its_directory() {
        let tmp = temp_dir();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::create_dir_all(tmp.path().join("home")).unwrap();
        symlink(&outside, tmp.path().join("home/codex")).unwrap();
        let err = PinnedDir::open(tmp.path(), Path::new("home/codex"), true)
            .err()
            .unwrap();
        assert!(
            matches!(err.raw_os_error(), Some(libc::ELOOP | libc::ENOTDIR)),
            "{err}"
        );
        assert!(PinnedDir::open(tmp.path(), Path::new("../x"), true).is_err());

        let dir = PinnedDir::open(tmp.path(), Path::new("d"), true).unwrap();
        let victim = tmp.path().join("victim");
        std::fs::write(&victim, b"host secret").unwrap();
        symlink(&victim, tmp.path().join("d/auth.json")).unwrap();
        assert!(dir.read("auth.json", 1024).is_err());
        assert!(dir.open_append("auth.json", 0o600).is_err());
        assert!(dir.read("../victim", 1024).is_err());
        dir.write_atomic("auth.json", b"new", 0o600).unwrap();
        assert_eq!(std::fs::read(&victim).unwrap(), b"host secret");
        assert_eq!(dir.read("auth.json", 1024).unwrap().unwrap(), b"new");
    }
}
