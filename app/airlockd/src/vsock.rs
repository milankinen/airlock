//! Minimal virtio-vsock listener.
//!
//! The kernel's `AF_VSOCK` socket family is used for host↔guest communication
//! without requiring network configuration. We use raw syscalls because the
//! standard library doesn't expose vsock support.
//!
//! `AF_VSOCK`, `SOCK_CLOEXEC` and `accept4` are Linux-only, so the
//! implementation lives in a private Linux-gated module with
//! compile-time stubs for other targets so `cargo check` works on
//! macOS — the same arrangement as `net`.

#[cfg(target_os = "linux")]
mod imp {
    use std::mem;
    use std::os::unix::io::{FromRawFd, OwnedFd};

    const AF_VSOCK: i32 = 40;
    const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;

    /// Kernel `sockaddr_vm` layout for `AF_VSOCK` sockets.
    #[repr(C)]
    #[allow(clippy::struct_field_names)]
    struct SockaddrVm {
        svm_family: u16,
        svm_reserved1: u16,
        svm_port: u32,
        svm_cid: u32,
        svm_flags: u8,
        svm_zero: [u8; 3],
    }

    /// Create a vsock listener bound to the given port, accepting from any CID.
    pub fn listen(port: u32) -> std::io::Result<OwnedFd> {
        unsafe {
            // SOCK_CLOEXEC so the listener fd is not inherited across exec into
            // container processes — otherwise an untrusted process could walk
            // /proc/self/fd and speak the host RPC protocol directly, escaping
            // airlockd's mediation.
            let fd = libc::socket(AF_VSOCK, libc::SOCK_STREAM | libc::SOCK_CLOEXEC, 0);
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            let fd = OwnedFd::from_raw_fd(fd);

            let addr = SockaddrVm {
                svm_family: AF_VSOCK as u16,
                svm_reserved1: 0,
                svm_port: port,
                svm_cid: VMADDR_CID_ANY,
                svm_flags: 0,
                svm_zero: [0; 3],
            };

            if libc::bind(
                std::os::unix::io::AsRawFd::as_raw_fd(&fd),
                (&raw const addr).cast::<libc::sockaddr>(),
                mem::size_of::<SockaddrVm>() as u32,
            ) < 0
            {
                return Err(std::io::Error::last_os_error());
            }

            if libc::listen(std::os::unix::io::AsRawFd::as_raw_fd(&fd), 1) < 0 {
                return Err(std::io::Error::last_os_error());
            }

            Ok(fd)
        }
    }

    /// Accept a single connection on a vsock listener.
    pub fn accept(listen_fd: &OwnedFd) -> std::io::Result<OwnedFd> {
        unsafe {
            // accept4 with SOCK_CLOEXEC: the connected fd carries the live host
            // RPC channel and must not leak into exec'd container processes.
            let fd = libc::accept4(
                std::os::unix::io::AsRawFd::as_raw_fd(listen_fd),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC,
            );
            if fd < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(OwnedFd::from_raw_fd(fd))
        }
    }
}

// --- Non-Linux stubs ------------------------------------------------
//
// airlockd is only ever executed inside the Linux guest VM. These
// stubs exist so the crate still type-checks on the host-side
// developer machine (macOS, etc.) without having to shard the build
// into per-target binaries.
#[cfg(not(target_os = "linux"))]
use std::os::unix::io::OwnedFd;

#[cfg(target_os = "linux")]
pub use imp::{accept, listen};

#[cfg(not(target_os = "linux"))]
pub fn listen(_port: u32) -> std::io::Result<OwnedFd> {
    unimplemented!("airlockd only runs inside the Linux VM");
}

#[cfg(not(target_os = "linux"))]
pub fn accept(_listen_fd: &OwnedFd) -> std::io::Result<OwnedFd> {
    unimplemented!("airlockd only runs inside the Linux VM");
}
