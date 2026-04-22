//! Minimal TUN (L3) device helper. Opens `/dev/net/tun`, sets `IFF_TUN |
//! IFF_NO_PI` via `TUNSETIFF`, flips the fd non-blocking, and exposes a
//! bare `read`/`write` API. Interface bring-up and addressing is left to
//! `init::linux::net` (which shells out to `/sbin/ip`).
//!
//! Why raw ioctl rather than a crate: the dependency surface is one
//! struct and one constant. A wrapper crate would add a dep for roughly
//! the lines below.
//!
//! The returned `Tun` keeps the file descriptor open for the lifetime of
//! the process — airlockd exits on VM shutdown, so we don't bother with
//! an explicit close path.
//!
//! # Safety
//!
//! `ioctl(TUNSETIFF)` writes into a stack `ifreq` we zero out first;
//! the kernel copies back the assigned name but we don't read it back.
//! Non-blocking is set with `fcntl(F_SETFL, O_NONBLOCK)`.
//!
//! # Notes
//!
//! - `IFF_NO_PI` means packets come through as raw IP — no 4-byte
//!   `tun_pi` prefix — which is what smoltcp expects when configured
//!   for medium-ip.
//! - The fd must be registered with `tokio::io::unix::AsyncFd` for
//!   readiness-driven polling; the wrapper in `smoltcp_proxy.rs`
//!   handles that.
//!
//! Reference: `linux/Documentation/networking/tuntap.rst`.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

const IFF_TUN: libc::c_short = 0x0001;
const IFF_NO_PI: libc::c_short = 0x1000;

// TUNSETIFF from <linux/if_tun.h> — 'T', 202, sizeof(int).
const TUNSETIFF: libc::Ioctl = 0x4004_54ca;

#[repr(C)]
#[derive(Clone, Copy)]
struct Ifreq {
    name: [libc::c_char; libc::IFNAMSIZ],
    flags: libc::c_short,
    _pad: [u8; 22],
}

pub struct Tun {
    file: File,
    name: String,
}

impl Tun {
    /// Open `/dev/net/tun` and create a TUN device named `name`. Sets the fd
    /// non-blocking. The device starts DOWN and unaddressed — caller runs
    /// `ip link set <name> up` + `ip addr add ...` separately.
    pub fn create(name: &str) -> io::Result<Self> {
        if name.len() >= libc::IFNAMSIZ {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tun name too long",
            ));
        }

        ensure_tun_dev()?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/net/tun")?;
        let fd = file.as_raw_fd();

        let mut ifr = Ifreq {
            name: [0; libc::IFNAMSIZ],
            flags: IFF_TUN | IFF_NO_PI,
            _pad: [0; 22],
        };
        // Copy requested name into the zeroed c-string slot.
        for (dst, b) in ifr.name.iter_mut().zip(name.bytes()) {
            *dst = b as libc::c_char;
        }

        let rc = unsafe { libc::ioctl(fd, TUNSETIFF, &raw mut ifr) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }

        set_nonblocking(fd)?;
        Ok(Self {
            file,
            name: name.to_string(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }

    pub fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.file.write(buf)
    }
}

impl AsRawFd for Tun {
    fn as_raw_fd(&self) -> RawFd {
        self.file.as_raw_fd()
    }
}

impl IntoRawFd for Tun {
    fn into_raw_fd(self) -> RawFd {
        self.file.into_raw_fd()
    }
}

impl FromRawFd for Tun {
    /// # Safety
    ///
    /// Caller guarantees `fd` refers to an open TUN file descriptor.
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self {
            file: unsafe { File::from_raw_fd(fd) },
            name: String::new(),
        }
    }
}

/// Create `/dev/net/tun` if missing. Devtmpfs populates most nodes
/// automatically, but without udev the `net/` subdirectory is not always
/// present, so we `mknod(10, 200)` it ourselves. Major/minor are fixed
/// for the TUN misc device — see `Documentation/networking/tuntap.rst`.
fn ensure_tun_dev() -> io::Result<()> {
    if std::path::Path::new("/dev/net/tun").exists() {
        return Ok(());
    }
    std::fs::create_dir_all("/dev/net")?;
    let path = std::ffi::CString::new("/dev/net/tun").unwrap();
    let mode = libc::S_IFCHR | 0o600;
    let dev = libc::makedev(10, 200);
    let rc = unsafe { libc::mknod(path.as_ptr(), mode, dev) };
    if rc < 0 {
        let err = io::Error::last_os_error();
        // EEXIST is fine — a racing thread got here first.
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err);
        }
    }
    Ok(())
}

fn set_nonblocking(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
