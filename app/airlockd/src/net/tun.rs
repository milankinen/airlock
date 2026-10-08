//! Virtual network device.
//!
//! Gives the guest a minimal TUN device that reads and writes raw IP packets.
//! The outgoing TCP proxy uses it to catch the traffic of the VM.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::io::{AsRawFd, FromRawFd, IntoRawFd, RawFd};

/// TUN (L3) device, not TAP.
const IFF_TUN: libc::c_short = 0x0001;
/// Packets are raw IP with no 4-byte `tun_pi` prefix. smoltcp expects this
/// format for medium-ip.
const IFF_NO_PI: libc::c_short = 0x1000;

// TUNSETIFF from <linux/if_tun.h>: 'T', 202, sizeof(int).
const TUNSETIFF: libc::Ioctl = 0x4004_54ca;

/// Kernel `ifreq` layout for `TUNSETIFF`.
#[repr(C)]
#[derive(Clone, Copy)]
struct Ifreq {
    name: [libc::c_char; libc::IFNAMSIZ],
    flags: libc::c_short,
    _pad: [u8; 22],
}

/// An open TUN device.
///
/// Keeps the fd open for the lifetime of the process. airlockd exits when
/// the VM stops, so there is no explicit close.
///
/// The fd is non-blocking. For readiness-driven polling, register it with
/// `tokio::io::unix::AsyncFd`.
pub struct Tun {
    file: File,
    name: String,
}

impl Tun {
    /// Open `/dev/net/tun` and create a non-blocking TUN device named
    /// `name`.
    ///
    /// The device starts DOWN and with no address. The caller must enable it
    /// and give it an address separately.
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

        // Use a raw ioctl and not a crate. The code needs only one struct and
        // one constant. Reference: `linux/Documentation/networking/tuntap.rst`.
        let mut ifr = Ifreq {
            name: [0; libc::IFNAMSIZ],
            flags: IFF_TUN | IFF_NO_PI,
            _pad: [0; 22],
        };
        // Copy requested name into the zeroed c-string slot.
        for (dst, b) in ifr.name.iter_mut().zip(name.bytes()) {
            *dst = b as libc::c_char;
        }

        // SAFETY: `ifr` is a valid, zero-initialized `ifreq` on the stack.
        // The kernel writes the assigned name back into it, but this function
        // does not read it.
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

    /// Interface name. Empty for a `Tun` made with `from_raw_fd`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Read one IP packet into `buf`. Returns `WouldBlock` if no packet is
    /// available.
    pub fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.file.read(buf)
    }

    /// Write one IP packet from `buf`.
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
    /// The caller must make sure that `fd` is an open TUN file descriptor.
    unsafe fn from_raw_fd(fd: RawFd) -> Self {
        Self {
            file: unsafe { File::from_raw_fd(fd) },
            name: String::new(),
        }
    }
}

/// Create `/dev/net/tun` if it does not exist.
///
/// devtmpfs creates most nodes automatically. But without udev, the `net/`
/// subdirectory is not always there. Then this function makes the node with
/// `mknod(10, 200)`.
/// The TUN misc device always has these major/minor numbers (see
/// `Documentation/networking/tuntap.rst`).
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
        // EEXIST is fine. Another thread created the node first.
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err);
        }
    }
    Ok(())
}

/// Set `O_NONBLOCK` on `fd` with `fcntl(F_SETFL)`.
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
