//! VM backend interface.
//!
//! A VM backend boots the sandbox VM from a platform-independent VM
//! description and gives a handle to control the running VM. The `vmm`
//! setting selects the backend. The sandbox process loads the backend before
//! it does other work, so a backend that the host cannot run stops the start
//! early.

use std::os::unix::io::OwnedFd;

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
    fn kernel_console(&self) -> &'static str;

    /// Get the id maps that guest init puts on the shares.
    /// Args:
    ///  - `container_uid`, `container_gid`: Ids of the container user
    ///
    /// Returns:
    ///   The maps for the image layers and for the other shares, or `None`
    ///   if the share server maps the owners itself.
    fn id_map(&self, container_uid: u32, container_gid: u32) -> Option<(IdMap, IdMap)>;

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

/// Id map for a share: swaps the host uid and gid with guest ids and keeps
/// all other ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IdMap {
    /// Host uid of the shared files.
    pub host_uid: u32,
    /// Host gid of the shared files.
    pub host_gid: u32,
    /// Guest uid that the host uid shows as.
    pub guest_uid: u32,
    /// Guest gid that the host gid shows as.
    pub guest_gid: u32,
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

/// Error of a VM handle when the VM stopped without a stop request. The
/// boot does not try again after it.
#[derive(Debug)]
pub struct VmStopped(pub String);

impl std::fmt::Display for VmStopped {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VmStopped {}

/// Load the VM backend `vmm`.
/// Returns:
///   The backend, or error if the host cannot run it.
pub fn load_backend(vmm: Vmm) -> anyhow::Result<&'static dyn VmmBackend> {
    match vmm {
        Vmm::AppleVirt => super::apple_virt::load(),
        Vmm::CloudHypervisor => super::cloud_hypervisor::load(),
        Vmm::Libkrun => super::libkrun::load(),
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
