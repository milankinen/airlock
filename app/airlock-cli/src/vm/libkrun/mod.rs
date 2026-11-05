//! libkrun VM backend.
//!
//! Runs the sandbox VM with libkrun in a separate VM process of the airlock
//! program. A Seatbelt sandbox locks the VM process to the shared
//! directories, the disk image and the vsock sockets before the guest runs.
//! Only macOS hosts on Apple silicon can run this backend.

// All backends use the same layout: the implementation file has the name of
// the backend.
#[cfg(vm_libkrun)]
#[allow(clippy::module_inception)]
mod libkrun;
#[cfg(vm_libkrun)]
mod seatbelt;
#[cfg(all(test, vm_libkrun))]
mod tests;
#[cfg(vm_libkrun)]
mod vmm;

#[cfg(vm_libkrun)]
pub use self::vmm::main as vmm_main;
use super::backend::VmmBackend;

/// Load the libkrun backend.
/// Returns:
///   The backend, or error if the host is not macOS on Apple silicon.
// All loaders have the same signature, also when they cannot fail.
#[allow(clippy::unnecessary_wraps)]
pub fn load() -> anyhow::Result<&'static dyn VmmBackend> {
    #[cfg(vm_libkrun)]
    {
        Ok(&libkrun::Libkrun)
    }
    #[cfg(not(vm_libkrun))]
    {
        Err(super::backend::unsupported(crate::settings::Vmm::Libkrun))
    }
}
