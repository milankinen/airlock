//! Apple Virtualization framework VM backend.
//!
//! Runs the sandbox VM with the Apple Virtualization framework. Only macOS
//! hosts can run this backend.

// All backends use the same layout: the implementation file has the name of
// the backend.
#[cfg(target_os = "macos")]
#[allow(clippy::module_inception)]
mod apple_virt;

use std::rc::Rc;

use super::backend::VmmBackend;

/// Load the Apple Virtualization framework backend.
/// Returns:
///   The backend, or error if the host is not macOS.
// All loaders have the same signature, also when they cannot fail.
#[allow(clippy::unnecessary_wraps)]
pub fn load() -> anyhow::Result<Rc<dyn VmmBackend>> {
    #[cfg(target_os = "macos")]
    {
        Ok(Rc::new(apple_virt::AppleVirt))
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(super::backend::unsupported(crate::settings::Vmm::AppleVirt))
    }
}
