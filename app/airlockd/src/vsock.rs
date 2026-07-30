//! Minimal virtio-vsock listener.
//!
//! The kernel's `AF_VSOCK` socket family is used for host↔guest communication
//! without requiring network configuration. We use raw syscalls because the
//! standard library doesn't expose vsock support.

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
