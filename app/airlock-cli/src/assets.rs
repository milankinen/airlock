//! VM boot assets.
//!
//! The `airlock` binary contains the files that boot the sandbox VM: the
//! kernel, the initramfs and, on Linux, the hypervisor. This module extracts
//! them to the cache on the host when they change. The `[vm]` config can
//! replace the bundled kernel and initramfs with custom files.

use std::path::{Path, PathBuf};

use crate::config::config_values::Mount;
use crate::messages;
use crate::project::Project;
use crate::util::{VerifiedPath, vfs};
use crate::vm::mount;

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

        let dir = project.context.roots.data_dir.join("vm")?;
        vfs::create_dir_all(&dir)?;

        // Serialize the checksum check and extraction across processes. Take
        // a blocking exclusive lock before the checksum read. Then two
        // concurrent first runs (for example after an upgrade) cannot both
        // write the boot assets. Also, the hypervisor never memory-maps a
        // file while another process writes it. The lock releases when
        // `_lock` drops.
        let _lock = acquire_extract_lock(&dir.join("lock")?)?;

        let checksum_file = dir.join("checksum")?;
        let cached_checksum = vfs::read_to_string(&checksum_file).unwrap_or_default();
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

            vfs::write(&checksum_file, CHECKSUM)?;
        }

        #[cfg(not(feature = "distroless"))]
        let bundled_kernel = Some(dir.join("Image")?);
        #[cfg(feature = "distroless")]
        let bundled_kernel = None;

        #[cfg(not(feature = "distroless"))]
        let bundled_initramfs = Some(dir.join("initramfs.gz")?);
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
            cloud_hypervisor: dir.join("cloud-hypervisor")?.to_path()?.to_path_buf(),
            #[cfg(target_os = "linux")]
            virtiofsd: dir.join("virtiofsd")?.to_path()?.to_path_buf(),
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
fn write_executable(dir: &VerifiedPath, name: &str, data: &[u8]) -> anyhow::Result<()> {
    let tmp = dir.join(format!(".{name}.tmp"))?;
    write_new(&tmp, data, 0o755)?;
    vfs::rename(&tmp, &dir.join(name)?)?;
    Ok(())
}

/// Write `data` to the new file `path` with the permission bits `mode`.
/// Removes an old file at `path` first. The create never follows a
/// symlink at `path`.
#[cfg(any(all(target_os = "linux", not(test)), not(feature = "distroless")))]
fn write_new(path: &VerifiedPath, data: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    // A temp file from a crashed run can remain.
    match vfs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let mut file = vfs::create_new(path)?;
    file.write_all(data)?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Write `data` to `dir/name` through a sibling temp file and rename. A
/// reader never sees a partly written file, and a concurrent process cannot
/// boot from it. Used for `Image` and `initramfs.gz`. Unlike
/// `write_executable`, it works on all platforms and does not set the
/// executable bit.
#[cfg(not(feature = "distroless"))]
fn write_atomic(dir: &VerifiedPath, name: &str, data: &[u8]) -> anyhow::Result<()> {
    // The temp name contains the pid, so a stray temp file of another
    // process never goes into place.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()))?;
    write_new(&tmp, data, 0o644)?;
    vfs::rename(&tmp, &dir.join(name)?)?;
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
fn acquire_extract_lock(path: &VerifiedPath) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let file = vfs::open_with(
        path,
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_NOFOLLOW),
    )?;
    let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) };
    if rc != 0 {
        anyhow::bail!(messages::app::asset_lock_failed(
            path,
            std::io::Error::last_os_error()
        ));
    }
    Ok(file)
}

/// Resolve the path of one VM asset.
/// Args:
///  - `custom`: Custom path from the project config, if set. Expands `~`,
///    resolves relative paths from the project directory and follows
///    symlinks. A path into the project directory must stay in it.
///  - `project`: Project for path expansion
///  - `bundled`: Path of the extracted bundled asset. `None` for
///    `distroless` builds, which then require `custom`.
///  - `name`: Asset name for error messages
///
/// Returns:
///   The resolved path of `custom` if set, else `bundled`. Error if the
///   custom file does not exist, its path leaves the project directory
///   through a symlink, or no path is available.
#[cfg(not(test))]
fn resolve_asset(
    custom: Option<&str>,
    project: &Project,
    bundled: Option<VerifiedPath>,
    name: &str,
) -> anyhow::Result<PathBuf> {
    let Some(raw) = custom else {
        let bundled =
            bundled.ok_or_else(|| anyhow::anyhow!(messages::app::asset_must_be_set(name)))?;
        return Ok(bundled.to_path()?.to_path_buf());
    };

    resolve_custom(
        raw,
        &project.host_home,
        &project.host_cwd,
        &crate::vm::mount::enabled_mounts(&project.config),
        name,
    )
}

/// Resolve the custom path of one VM asset.
/// Args:
///  - `raw`: Path from the project config
///  - `host_home`: Host home for `~` expansion
///  - `cwd`: Project directory. Relative paths resolve from it
///  - `mounts`: Enabled mounts of the project config
///  - `name`: Asset name for error messages
///
/// Returns:
///   The resolved path, or error if the file does not exist, or if its path
///   goes into a directory that the sandbox can write and a symlink there
///   points out of it.
fn resolve_custom(
    raw: &str,
    host_home: &Path,
    cwd: &Path,
    mounts: &[(&str, Mount)],
    name: &str,
) -> anyhow::Result<PathBuf> {
    let path = mount::absolute(raw, host_home, cwd);
    // The user can pick a symlink, for example to the current kernel build.
    let resolved = VerifiedPath::new_follow(&path, None)?;
    // The sandbox can write the project dir and the writable dir mounts,
    // and put symlinks there. The project share is not known here, so the
    // project dir counts as writable.
    let sources = mount::resolve_sources(mounts, host_home, cwd)?;
    let writable = mount::writable_dirs(Some(cwd), &sources)?;
    mount::check_stays_in(
        &messages::vm::custom_asset_kind(name),
        &path,
        &resolved,
        &writable,
    )?;
    match vfs::metadata(&resolved) {
        Ok(_) => Ok(resolved.to_path()?.to_path_buf()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!(messages::app::custom_asset_not_found(name, path.display()))
        }
        Err(e) => Err(e.into()),
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the atomic write of the bundled VM assets and for the
    //! custom asset paths.

    use super::resolve_custom;
    use crate::test_cfg::temp_dir;

    /// Test that a custom kernel path through a symlink in the project is
    /// refused when the link points out of the project. The sandbox can
    /// change such a link and give a host file to the VM as its kernel.
    ///   1. Make a kernel file outside the project and a link to it in the
    ///      project
    ///   2. Resolve the link as the custom kernel and check the refusal
    ///   3. Resolve the real path and check that it is allowed
    #[test]
    fn custom_kernel_through_project_link_to_outside_is_refused() {
        let tmp = temp_dir();
        let root = std::fs::canonicalize(tmp.path()).unwrap();
        let project = root.join("project");
        std::fs::create_dir(&project).unwrap();
        let kernel = root.join("Image");
        std::fs::write(&kernel, b"kernel").unwrap();
        std::os::unix::fs::symlink(&kernel, project.join("Image")).unwrap();

        let err = resolve_custom("Image", &root, &project, &[], "kernel")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Set the real path"), "{err}");
        let real = kernel.to_str().unwrap();
        assert_eq!(
            resolve_custom(real, &root, &project, &[], "kernel").unwrap(),
            kernel
        );
    }
    /// Test that an asset write replaces the old file and leaves no temporary
    /// file, so that a reader never sees a partial asset.
    ///   1. Write an asset, then write it again with longer content
    ///   2. Check that the file has the new content
    ///   3. Check that the directory has only the asset file
    #[cfg(not(feature = "distroless"))]
    #[test]
    fn asset_write_replaces_file_and_leaves_no_temp_file() {
        use super::write_atomic;
        use crate::util::{Roots, VerifiedPath};

        let tmp = temp_dir();
        let dir = tmp.path();
        let roots = Roots::resolve(dir, dir, dir).unwrap();
        let verified = VerifiedPath::new(dir, &roots).unwrap();

        write_atomic(&verified, "Image", b"kernel-bytes").unwrap();
        write_atomic(&verified, "Image", b"newer-and-longer-bytes").unwrap();

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
