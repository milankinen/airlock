//! Cloud Hypervisor VM backend.
//!
//! Runs the sandbox VM with Cloud Hypervisor and shares host directories
//! with virtiofsd. Only Linux hosts can run this backend.

// All backends use the same layout: the implementation file has the name of
// the backend.
#[cfg(target_os = "linux")]
#[allow(clippy::module_inception)]
mod cloud_hypervisor;

use std::rc::Rc;

use super::backend::VmmBackend;

/// Load the Cloud Hypervisor backend.
/// Returns:
///   The backend, or error if the host is not Linux.
// All loaders have the same signature, also when they cannot fail.
#[allow(clippy::unnecessary_wraps)]
pub fn load() -> anyhow::Result<Rc<dyn VmmBackend>> {
    #[cfg(target_os = "linux")]
    {
        Ok(Rc::new(cloud_hypervisor::CloudHypervisor))
    }
    #[cfg(not(target_os = "linux"))]
    {
        Err(super::backend::unsupported(
            crate::settings::Vmm::CloudHypervisor,
        ))
    }
}
