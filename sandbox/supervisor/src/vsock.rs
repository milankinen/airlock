use std::mem;

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

pub fn listen(port: u32) -> std::io::Result<i32> {
    unsafe {
        let fd = libc::socket(AF_VSOCK, libc::SOCK_STREAM, 0);
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }

        let addr = SockaddrVm {
            svm_family: AF_VSOCK as u16,
            svm_reserved1: 0,
            svm_port: port,
            svm_cid: VMADDR_CID_ANY,
            svm_flags: 0,
            svm_zero: [0; 3],
        };

        if libc::bind(fd, &addr as *const _ as *const libc::sockaddr, mem::size_of::<SockaddrVm>() as u32) < 0 {
            libc::close(fd);
            return Err(std::io::Error::last_os_error());
        }

        if libc::listen(fd, 1) < 0 {
            libc::close(fd);
            return Err(std::io::Error::last_os_error());
        }

        Ok(fd)
    }
}

pub fn accept(listen_fd: i32) -> std::io::Result<i32> {
    unsafe {
        let fd = libc::accept(listen_fd, std::ptr::null_mut(), std::ptr::null_mut());
        if fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(fd)
    }
}
