//! Symlink-safe host file access, for directories the sandbox can write to.
//!
//! Some host directories are mounted read-write into the guest
//! (the agent packs' `~/.cache/airlock/packs/mounts/codex/codex`,
//! `~/.cache/airlock/packs/mounts/claude/claude`), and the guest can
//! plant symlinks in them. Code that later reads or writes files there on
//! the host must not follow such a link to a path of the guest's choosing.
//!
//! A [`PinnedDir`] holds an open descriptor of one directory. Every
//! operation is an `*at()` syscall relative to that descriptor with
//! `O_NOFOLLOW`, so neither a symlink at the file name nor a later swap of
//! a directory on the way can redirect it. Files that are read or written
//! must be regular files owned by the current user. Writes go through an
//! `O_EXCL` temp file and `renameat`, or truncate the file in place when
//! its inode must be kept (a hard-linked file mount).

use std::ffi::{CString, OsStr};
use std::fs::File;
use std::io::{self, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::os::unix::io::{AsRawFd, FromRawFd, OwnedFd};
use std::path::{Component, Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

/// An open directory; all file operations resolve relative to it and
/// never follow a symlink at the final component.
pub struct PinnedDir {
    fd: OwnedFd,
    /// The path the directory was opened at. Messages only; never passed
    /// to a syscall.
    display: PathBuf,
}

impl PinnedDir {
    /// Pin the directory at `path`. Only the last component is checked
    /// for a symlink; the components before it are resolved normally.
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

    /// Open `root/rel`. `root` is trusted and resolved normally (it may
    /// contain symlinks, e.g. `/tmp` on macOS). Each component of `rel` is
    /// opened with `O_NOFOLLOW` and must be a directory owned by the
    /// current user. With `create`, missing components are created with
    /// mode 0700; without it, a missing component is `NotFound`.
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

    /// Open the child directory `name`, creating it (0700) when `create`
    /// is set and it does not exist.
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

    /// The path this directory was opened at, for messages.
    pub fn path(&self) -> &Path {
        &self.display
    }

    /// Read the regular file `name`, at most `cap` bytes. `Ok(None)` when
    /// it does not exist; a symlink, a non-regular file, a foreign owner
    /// or a file larger than `cap` is an error.
    pub fn read(&self, name: impl AsRef<OsStr>, cap: u64) -> io::Result<Option<Vec<u8>>> {
        let name = name.as_ref();
        let c = file_name(name)?;
        // O_NONBLOCK: opening a planted FIFO must not block; the type
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

    /// Replace `name` atomically: write a fresh `O_EXCL` temp file with
    /// `mode`, sync it, and `renameat` it over `name`. A symlink at `name`
    /// is replaced, not followed.
    pub fn write_atomic(&self, name: impl AsRef<OsStr>, bytes: &[u8], mode: u32) -> io::Result<()> {
        let name = name.as_ref();
        self.replace_with(name, mode, |out| out.write_all(bytes))
    }

    /// Open `name` for appending, creating it with `mode` when absent.
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

    /// Unlink `name` (a file or a symlink, never followed). `Ok(false)`
    /// when it did not exist.
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

    /// The inode of `name`, without following a symlink. `None` when it
    /// does not exist or cannot be examined.
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

    /// Make `name` a hard link to the open file `src`, atomically (link
    /// to a temp name, then rename). Linux only: it links through
    /// `/proc/self/fd`, so the link targets exactly the inode `src` has
    /// open. Elsewhere this is `Unsupported`.
    #[cfg(target_os = "linux")]
    pub fn link_from(&self, src: &File, name: impl AsRef<OsStr>) -> io::Result<()> {
        let name = name.as_ref();
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

    /// macOS has no `/proc/self/fd` and no `linkat(AT_EMPTY_PATH)`, so a
    /// hard link cannot be made from an open file.
    #[cfg(not(target_os = "linux"))]
    #[allow(clippy::unused_self)] // the same signature as on Linux
    pub fn link_from(&self, _src: &File, _name: impl AsRef<OsStr>) -> io::Result<()> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "linkat from fd not available on this platform",
        ))
    }

    /// Replace `name` atomically with the current content of the open
    /// file `src` (copied from the descriptor, not a path).
    pub fn copy_from(&self, src: &File, name: impl AsRef<OsStr>, mode: u32) -> io::Result<()> {
        let mut src = src;
        self.replace_with(name.as_ref(), mode, |out| {
            io::copy(&mut src, out).map(|_| ())
        })
    }

    /// Write a temp file with `fill`, sync it and rename it over `name`.
    /// The temp file is removed on any failure.
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

    /// Rename the temp file `tmp` over `name`; remove it when that fails.
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

/// `openat` with `O_CLOEXEC`, returning an owned descriptor.
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

/// A single path component as a C string: no `/`, not empty, `.` or `..`.
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

/// A per-process unique temp name next to `name`.
fn temp_name(name: &OsStr) -> CString {
    static NEXT: AtomicU32 = AtomicU32::new(0);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    let mut bytes = b".".to_vec();
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(format!(".{}.{n}.airlock-tmp", std::process::id()).as_bytes());
    CString::new(bytes).expect("file names have no NUL bytes")
}

/// Files and directories must belong to the user airlock runs as.
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
    use std::os::unix::fs::{PermissionsExt, symlink};

    use super::*;
    use crate::test_cfg::temp_dir;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

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
