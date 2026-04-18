//! Embedded VM assets (kernel, initramfs, hypervisor binaries).
//!
//! These files are compiled into the `airlock` binary via `include_bytes!`. On
//! first run (or after a build changes the checksum), they are extracted to
//! `~/.cache/airlock/kernel/` so the hypervisor can memory-map them.
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
        const CHECKSUM: &str = env!("AIRLOCK_ASSETS_CHECKSUM");

        let dir = crate::cache::cache_dir()?.join("kernel");
        let checksum_file = dir.join("checksum");

        let cached_checksum = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        if cached_checksum.trim() != CHECKSUM {
            std::fs::create_dir_all(&dir)?;

            #[cfg(not(feature = "distroless"))]
            {
                std::fs::write(
                    dir.join("Image"),
                    include_bytes!("../../../target/vm/Image"),
                )?;
                std::fs::write(
                    dir.join("initramfs.gz"),
                    include_bytes!("../../../target/vm/initramfs.gz"),
                )?;
            }

            #[cfg(target_os = "linux")]
            {
                // Write to temp files first, then rename — avoids ETXTBSY if a
                // previous virtiofsd/cloud-hypervisor process is still running.
                write_executable(
                    &dir,
                    "cloud-hypervisor",
                    include_bytes!("../../../target/vm/cloud-hypervisor"),
                )?;
                write_executable(
                    &dir,
                    "virtiofsd",
                    include_bytes!("../../../target/vm/virtiofsd"),
                )?;
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

/// Write an executable to `dir/name` via a temp file + rename to avoid ETXTBSY.
#[cfg(all(target_os = "linux", not(test)))]
fn write_executable(dir: &std::path::Path, name: &str, data: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, data)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
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
