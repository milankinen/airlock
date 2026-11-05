//! VM backend interface.
//!
//! A VM backend boots the sandbox VM from a platform-independent VM
//! description and gives a handle to control the running VM. The `vmm`
//! setting selects the backend. The sandbox process loads the backend before
//! it does other work, so a backend that the host cannot run stops the start
//! early.

use std::os::unix::io::OwnedFd;
use std::rc::Rc;

use super::config::VmConfig;
use crate::messages;
use crate::settings::Vmm;

/// Future returned by the object-safe [`VmmBackend`] and [`VmHandle`]
/// methods.
pub type HandleFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + 'a>>;

/// A VM backend that the host can run.
pub trait VmmBackend {
    /// Get the `vmm` setting value of the backend.
    fn kind(&self) -> Vmm;

    /// Get the console part of the guest kernel command line.
    fn kernel_console(&self) -> &'static str {
        "console=hvc0 console=ttyS0"
    }

    /// Extract the assets of the backend and boot the VM. The caller must
    /// wait until a vsock port is ready, see
    /// [`super::VmInstance::vsock_connect`].
    /// Args:
    ///  - `config`: The VM to boot
    ///
    /// Returns:
    ///   The handle of the running VM, or error if the boot fails.
    fn boot_vm<'a>(
        &'a self,
        config: &'a VmConfig,
    ) -> HandleFuture<'a, anyhow::Result<Box<dyn VmHandle>>>;
}

/// Handle of a running VM. Dropping the handle kills the VM.
pub trait VmHandle {
    /// Open a new vsock connection to the given guest port. Used for the
    /// supervisor channel and for later channels, for example the network
    /// proxy.
    fn vsock_connect(&self, port: u32) -> HandleFuture<'_, anyhow::Result<OwnedFd>>;

    /// Stop the VM and wait until the backend confirms the stop. A drop of
    /// the handle after this does not stop the VM again.
    fn stop(&mut self) -> HandleFuture<'_, anyhow::Result<()>>;
}

/// Load the VM backend `vmm`.
/// Returns:
///   The backend, or error if the host cannot run it.
pub fn load_backend(vmm: Vmm) -> anyhow::Result<Rc<dyn VmmBackend>> {
    match vmm {
        Vmm::AppleVirt => super::apple_virt::load(),
        Vmm::CloudHypervisor => super::cloud_hypervisor::load(),
        // TODO: load the libkrun backend when it is available.
        Vmm::Libkrun => Err(unsupported(vmm)),
    }
}

/// Make the error for a backend `vmm` that the host cannot run.
pub(super) fn unsupported(vmm: Vmm) -> anyhow::Error {
    anyhow::anyhow!(messages::vm::vmm_unsupported(
        vmm,
        std::env::consts::OS,
        std::env::consts::ARCH
    ))
}
