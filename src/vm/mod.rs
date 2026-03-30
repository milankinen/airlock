#[cfg(target_os = "macos")]
pub mod apple;
pub mod config;

use crate::error::Result;
use std::os::unix::io::RawFd;

#[allow(dead_code)]
pub trait VmBackend {
    /// Boot the VM. Returns when the VM is running.
    fn start(&mut self) -> impl std::future::Future<Output = Result<()>>;

    /// Shut down the VM. Returns when the VM has stopped.
    fn stop(&mut self) -> impl std::future::Future<Output = Result<()>>;

    /// Wait until the VM has stopped (e.g. guest poweroff).
    fn wait_for_stop(&self) -> impl std::future::Future<Output = ()>;

    /// Get the file descriptors for console I/O.
    /// Returns (write_to_guest_fd, read_from_guest_fd).
    fn console_fds(&self) -> (RawFd, RawFd);
}
