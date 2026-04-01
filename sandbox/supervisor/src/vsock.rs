use std::mem;
use std::os::unix::io::{FromRawFd, OwnedFd};

const AF_VSOCK: i32 = 40;
const VMADDR_CID_ANY: u32 = 0xFFFFFFFF;

#[repr(C)]
struct SockaddrVm {
    svm_family: u16,
    svm_reserved1: u16,
    svm_port: u32,
    svm_cid: u32,
    svm_flags: u8,
    svm_zero: [u8; 3],
}

pub fn listen(port: u32) -> std::io::Result<OwnedFd> {
    unsafe {
        let fd = libc::socket(AF_VSOCK, libc::SOCK_STREAM, 0);
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
            &addr as *const _ as *const libc::sockaddr,
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

pub fn accept(listen_fd: &OwnedFd) -> std::io::Result<OwnedFd> {
    unsafe {
        let fd = libc::accept(
            std::os::unix::io::AsRawFd::as_raw_fd(listen_fd),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        );
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(OwnedFd::from_raw_fd(fd))
    }
}
