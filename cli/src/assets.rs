//! Embedded VM assets (kernel, initramfs, hypervisor binaries).
//!
//! These files are compiled into the `ez` binary via `include_bytes!`. On
//! first run (or after a build changes the checksum), they are extracted to
//! `~/.ezpez/kernel/` so the hypervisor can memory-map them.
//!
//! Custom kernel/initramfs paths can be set in `[vm]` config; when present
//! they override the bundled files.

use std::path::PathBuf;

use crate::project::Project;

/// Paths to the extracted VM boot assets.
pub struct Assets {
    pub kernel: PathBuf,
    pub initramfs: PathBuf,
    #[cfg(target_os = "linux")]
    pub cloud_hypervisor: PathBuf,
    #[cfg(target_os = "linux")]
    pub virtiofsd: PathBuf,
}

impl Assets {
    /// Extract embedded assets to the cache directory if the checksum changed,
    /// then apply any custom kernel/initramfs paths from the project config.
    ///
    /// With the `distroless` feature, kernel and initramfs are not bundled —
    /// `vm.kernel` and `vm.initramfs` must be set in the project config.
    #[cfg(not(test))]
    pub fn init(project: &Project) -> anyhow::Result<Assets> {
        const CHECKSUM: &str = env!("EZPEZ_ASSETS_CHECKSUM");

        let dir = crate::cache::cache_dir()?.join("kernel");
        let checksum_file = dir.join("checksum");

        let cached_checksum = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        if cached_checksum.trim() != CHECKSUM {
            std::fs::create_dir_all(&dir)?;

            #[cfg(not(feature = "distroless"))]
            {
                std::fs::write(dir.join("Image"), include_bytes!("../../sandbox/out/Image"))?;
                std::fs::write(
                    dir.join("initramfs.gz"),
                    include_bytes!("../../sandbox/out/initramfs.gz"),
                )?;
            }

            #[cfg(target_os = "linux")]
            {
                use std::os::unix::fs::PermissionsExt;

                let ch = dir.join("cloud-hypervisor");
                std::fs::write(&ch, include_bytes!("../../sandbox/out/cloud-hypervisor"))?;
                std::fs::set_permissions(&ch, std::fs::Permissions::from_mode(0o755))?;

                let vfs = dir.join("virtiofsd");
                std::fs::write(&vfs, include_bytes!("../../sandbox/out/virtiofsd"))?;
                std::fs::set_permissions(&vfs, std::fs::Permissions::from_mode(0o755))?;
            }

            std::fs::write(&checksum_file, CHECKSUM)?;
        }

        #[cfg(not(feature = "distroless"))]
        let bundled_kernel = Some(dir.join("Image"));
        #[cfg(feature = "distroless")]
        let bundled_kernel = None;

        #[cfg(not(feature = "distroless"))]
        let bundled_initramfs = Some(dir.join("initramfs.gz"));
        #[cfg(feature = "distroless")]
        let bundled_initramfs = None;

        let kernel = resolve_asset(
            project.config.vm.kernel.as_deref(),
            project,
            bundled_kernel,
            "kernel",
        )?;
        let initramfs = resolve_asset(
            project.config.vm.initramfs.as_deref(),
            project,
            bundled_initramfs,
            "initramfs",
        )?;

        Ok(Assets {
            kernel,
            initramfs,
            #[cfg(target_os = "linux")]
            cloud_hypervisor: dir.join("cloud-hypervisor"),
            #[cfg(target_os = "linux")]
            virtiofsd: dir.join("virtiofsd"),
        })
    }

    #[cfg(test)]
    pub fn init(_project: &Project) -> anyhow::Result<Assets> {
        anyhow::bail!("Assets::init not supported in tests")
    }
}

/// Resolve an asset path: use `custom` if provided (with tilde expansion and
/// existence check), otherwise fall back to `bundled`.
///
/// `bundled` is `None` for `distroless` builds — `custom` is then required.
#[cfg(not(test))]
fn resolve_asset(
    custom: Option<&str>,
    project: &Project,
    bundled: Option<PathBuf>,
    name: &str,
) -> anyhow::Result<PathBuf> {
    let Some(raw) = custom else {
        return bundled.ok_or_else(|| {
            anyhow::anyhow!(
                "vm.{name} must be set in config (this is a distroless build with no bundled {name})"
            )
        });
    };

    let path = project.expand_host_tilde(raw);
    let path = if path.is_relative() {
        project.host_cwd.join(path)
    } else {
        path
    };

    if !path.exists() {
        anyhow::bail!("custom {name} not found: {}", path.display());
    }

    Ok(path)
}
