//! Apple Virtualization framework VM backend.
//!
//! Runs the sandbox VM with the Apple Virtualization framework. macOS only.

// All backends name the implementation file like the backend.
#[cfg(target_os = "macos")]
#[allow(clippy::module_inception)]
mod apple_virt;

use super::backend::VmmBackend;

/// Load the Apple Virtualization framework backend.
/// Returns:
///   The backend, or error if the host is not macOS.
// All loaders have the same signature.
#[allow(clippy::unnecessary_wraps)]
pub fn load() -> anyhow::Result<&'static dyn VmmBackend> {
    #[cfg(target_os = "macos")]
    {
        Ok(&apple_virt::AppleVirt)
    }
    #[cfg(not(target_os = "macos"))]
    {
        Err(super::backend::unsupported(crate::settings::Vmm::AppleVirt))
    }
}
