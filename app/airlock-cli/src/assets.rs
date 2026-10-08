//! VM boot assets.
//!
//! The `airlock` binary contains the files that boot the sandbox VM: the
//! kernel, the initramfs and, on Linux, the hypervisor. This module extracts
//! them to the cache on the host when they change. The `[vm]` config can
//! replace the bundled kernel and initramfs with custom files.

use std::path::PathBuf;

use crate::project::Project;

/// Paths to the extracted VM boot assets.
pub struct Assets {
    /// Kernel image.
    pub kernel: PathBuf,
    /// Initramfs archive.
    pub initramfs: PathBuf,
    /// `cloud-hypervisor` executable.
    #[cfg(target_os = "linux")]
    pub cloud_hypervisor: PathBuf,
    /// `virtiofsd` executable.
    #[cfg(target_os = "linux")]
    pub virtiofsd: PathBuf,
}

impl Assets {
    /// Get the VM asset paths for the project.
    ///
    /// Extracts the embedded assets to the cache directory if their checksum
    /// changed. The custom kernel and initramfs paths of the project config
    /// replace the bundled files. With the `distroless` feature, the binary
    /// does not contain a kernel and initramfs, so the project config must
    /// set `vm.kernel` and `vm.initramfs`.
    /// Returns:
    ///   The asset paths, or error if extraction fails or an asset is missing.
    #[cfg(not(test))]
    pub fn init(project: &Project) -> anyhow::Result<Assets> {
        const CHECKSUM: &str = env!("AIRLOCK_ASSETS_CHECKSUM");

        let dir = crate::cache::data_dir()?.join("vm");
        std::fs::create_dir_all(&dir)?;

        // Serialize the checksum check and extraction across processes. Take
        // a blocking exclusive lock before the checksum read. Then two
        // concurrent first runs (for example after an upgrade) cannot both
        // write the boot assets. Also, the hypervisor never memory-maps a
        // file while another process writes it. The lock releases when
        // `_lock` drops.
        let _lock = acquire_extract_lock(&dir.join("lock"))?;

        let checksum_file = dir.join("checksum");
        let cached_checksum = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        if cached_checksum.trim() != CHECKSUM {
            #[cfg(not(feature = "distroless"))]
            {
                // Write through a temp file and rename, so a reader (or a second
                // process) never sees a partly written Image or initramfs.
                write_atomic(&dir, "Image", include_bytes!("../../../target/vm/Image"))?;
                write_atomic(
                    &dir,
                    "initramfs.gz",
                    include_bytes!("../../../target/vm/initramfs.gz"),
                )?;
            }

            #[cfg(target_os = "linux")]
            {
                // Write to temp files first, then rename. This prevents ETXTBSY
                // if a previous virtiofsd/cloud-hypervisor process still runs.
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

    /// Test stub. Tests do not extract the VM assets, so this always fails.
    #[cfg(test)]
    pub fn init(_project: &Project) -> anyhow::Result<Assets> {
        anyhow::bail!("Assets::init not supported in tests")
    }
}

/// Write an executable to `dir/name` through a temp file and rename. This
/// prevents ETXTBSY if the old executable still runs.
#[cfg(all(target_os = "linux", not(test)))]
fn write_executable(dir: &std::path::Path, name: &str, data: &[u8]) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let tmp = dir.join(format!(".{name}.tmp"));
    std::fs::write(&tmp, data)?;
    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o755))?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

/// Write `data` to `dir/name` through a sibling temp file and rename. A
/// reader never sees a partly written file, and a concurrent process cannot
/// boot from it. Used for `Image` and `initramfs.gz`. Unlike
/// `write_executable`, it works on all platforms and does not set the
/// executable bit.
#[cfg(not(feature = "distroless"))]
fn write_atomic(dir: &std::path::Path, name: &str, data: &[u8]) -> anyhow::Result<()> {
    // The temp name contains the pid, so a stray temp file of another
    // process never goes into place.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

/// Get a blocking exclusive advisory lock on `path`. The lock stays until the
/// returned handle drops. It serializes the checksum check and extraction in
/// [`Assets::init`] across processes.
///
/// The lock blocks (and does not fail fast) because extraction is short. A
/// second process waits, then reads the new checksum and does not extract
/// again. Same `flock` pattern as `project::acquire_lock` and
/// `vault::acquire_file_lock`.
#[cfg(not(test))]
fn acquire_extract_lock(path: &std::path::Path) -> anyhow::Result<std::fs::File> {
    use std::os::unix::io::AsRawFd;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        anyhow::bail!(
            "failed to lock VM asset cache {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(file)
}

/// Resolve the path of one VM asset.
/// Args:
///  - `custom`: Custom path from the project config, if set. Expands `~` and
///    resolves relative paths from the project directory.
///  - `project`: Project for path expansion
///  - `bundled`: Path of the extracted bundled asset. `None` for
///    `distroless` builds, which then require `custom`.
///  - `name`: Asset name for error messages
///
/// Returns:
///   `custom` if set, else `bundled`. Error if the custom file does not
///   exist or no path is available.
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

#[cfg(all(test, not(feature = "distroless")))]
mod tests {
    //! Tests for the atomic write of the bundled VM assets.

    use super::write_atomic;
    use crate::test_cfg::temp_dir;

    /// Test that an asset write replaces the old file and leaves no temporary
    /// file, so that a reader never sees a partial asset.
    ///   1. Write an asset, then write it again with longer content
    ///   2. Check that the file has the new content
    ///   3. Check that the directory has only the asset file
    #[test]
    fn asset_write_replaces_file_and_leaves_no_temp_file() {
        let tmp = temp_dir();
        let dir = tmp.path();

        write_atomic(dir, "Image", b"kernel-bytes").unwrap();
        write_atomic(dir, "Image", b"newer-and-longer-bytes").unwrap();

        assert_eq!(
            std::fs::read(dir.join("Image")).unwrap(),
            b"newer-and-longer-bytes"
        );
        let names: Vec<_> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, ["Image"]);
    }
}
