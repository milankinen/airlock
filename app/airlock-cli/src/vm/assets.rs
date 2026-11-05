//! VM boot assets.
//!
//! The `airlock` binary contains the files that boot the sandbox VM: the
//! guest kernel and initramfs, and the files of the VM backend, for example
//! the hypervisor. The files come in groups. This module extracts a group to
//! the cache on the host when its content changes. Each VM backend extracts
//! its own group. The `[vm]` config can replace the bundled kernel and
//! initramfs with custom files.

// Builds with no bundled kernel and no backend group do not extract.
#![cfg_attr(
    all(feature = "distroless", not(target_os = "linux")),
    allow(dead_code)
)]

use crate::config::config_values::Mount;
use crate::messages;
use crate::sandbox::Project;
use crate::util::VerifiedPath;
use crate::vm::mount;

/// Paths of the VM boot files.
pub struct BootFiles {
    /// Directory of the extracted assets. VM backends extract their own
    /// assets here.
    pub dir: VerifiedPath,
    /// Kernel image.
    pub kernel: VerifiedPath,
    /// Initramfs archive.
    pub initramfs: VerifiedPath,
}

/// One file of an asset group.
#[cfg(not(test))]
pub struct AssetFile {
    /// File name in the asset directory.
    pub name: &'static str,
    /// Content of the file.
    pub data: &'static [u8],
    /// Permission bits of the file, for example `0o755` for an executable.
    pub mode: u32,
}

/// Get the VM boot files for the project.
///
/// Extracts the bundled kernel and initramfs to the asset directory if
/// their checksum changed. The custom kernel and initramfs paths of the
/// project config replace the bundled files. With the `distroless` feature,
/// the binary does not contain a kernel and initramfs, so the project config
/// must set `vm.kernel` and `vm.initramfs`.
/// Returns:
///   The file paths, or error if extraction fails or a file is missing.
#[cfg(not(test))]
pub fn boot_files(project: &Project) -> anyhow::Result<BootFiles> {
    let dir = crate::data_dir::make_dir(&project.context.data.vm)?;

    #[cfg(not(feature = "distroless"))]
    let (bundled_kernel, bundled_initramfs) = {
        extract_group(
            &dir,
            "kernel",
            env!("AIRLOCK_KERNEL_CHECKSUM"),
            &[
                AssetFile {
                    name: "Image",
                    data: include_bytes!("../../../../target/vm/Image"),
                    mode: 0o644,
                },
                AssetFile {
                    name: "initramfs.gz",
                    data: include_bytes!("../../../../target/vm/initramfs.gz"),
                    mode: 0o644,
                },
            ],
        )?;
        (Some(dir.join("Image")?), Some(dir.join("initramfs.gz")?))
    };
    #[cfg(feature = "distroless")]
    let (bundled_kernel, bundled_initramfs) = (None, None);

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
    Ok(BootFiles {
        dir,
        kernel,
        initramfs,
    })
}

/// Test stub. Tests do not extract the VM assets, so this always fails.
#[cfg(test)]
pub fn boot_files(_project: &Project) -> anyhow::Result<BootFiles> {
    anyhow::bail!("vm::assets::boot_files not supported in tests")
}

/// Extract the asset group `group` to `dir` if its checksum changed.
/// Args:
///  - `dir`: The asset directory
///  - `group`: Name of the group. The checksum of the group goes to the
///    file `checksum-<group>`
///  - `checksum`: Checksum of the bundled group, from the build script
///  - `files`: The files of the group
///
/// Returns:
///   `Ok` when the files of the group are in place, or error if the
///   extraction fails.
#[cfg(not(test))]
pub fn extract_group(
    dir: &VerifiedPath,
    group: &str,
    checksum: &str,
    files: &[AssetFile],
) -> anyhow::Result<()> {
    // Serialize the checksum check and extraction across processes. Take a
    // blocking exclusive lock before the checksum read. Then two concurrent
    // first runs (for example after an upgrade) cannot both write the
    // assets. Also, the hypervisor never memory-maps a file while another
    // process writes it. The lock releases when `_lock` drops.
    let _lock = acquire_extract_lock(&dir.join("lock")?)?;

    let checksum_file = dir.join(format!("checksum-{group}"))?;
    let cached_checksum = checksum_file.read_to_string().unwrap_or_default();
    if cached_checksum.trim() != checksum {
        for file in files {
            write_atomic(dir, file.name, file.data, file.mode)?;
        }
        checksum_file.write(checksum)?;
    }
    Ok(())
}

/// Write `data` to the new file `path` with the permission bits `mode`.
/// Removes an old file at `path` first. The create never follows a
/// symlink at `path`.
fn write_new(path: &VerifiedPath, data: &[u8], mode: u32) -> anyhow::Result<()> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;

    // A temp file from a crashed run can remain.
    match path.remove_file() {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => return Err(e.into()),
        _ => {}
    }
    let mut file = path.create_new()?;
    file.write_all(data)?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Write `data` to `dir/name` with the permission bits `mode`, through a
/// sibling temp file and rename. A reader never sees a partly written file,
/// and a concurrent process cannot boot from it. The rename also prevents
/// ETXTBSY if an old executable at `dir/name` still runs, for example a
/// virtiofsd or hypervisor process of a previous version.
fn write_atomic(dir: &VerifiedPath, name: &str, data: &[u8], mode: u32) -> anyhow::Result<()> {
    // The temp name contains the pid, so a stray temp file of another
    // process never goes into place.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()))?;
    write_new(&tmp, data, mode)?;
    tmp.rename_to(&dir.join(name)?)?;
    Ok(())
}

/// Get a blocking exclusive advisory lock on `path`. The lock stays until the
/// returned handle drops. It serializes the checksum check and extraction in
/// [`extract_group`] across processes.
///
/// The lock blocks (and does not fail fast) because extraction is short. A
/// second process waits, then reads the new checksum and does not extract
/// again. Same `flock` pattern as `sandbox::acquire_lock` and
/// `vault::acquire_file_lock`.
#[cfg(not(test))]
fn acquire_extract_lock(path: &VerifiedPath) -> anyhow::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    use std::os::unix::io::AsRawFd;

    let file = path.open_with(
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
) -> anyhow::Result<VerifiedPath> {
    let Some(raw) = custom else {
        return bundled.ok_or_else(|| anyhow::anyhow!(messages::app::asset_must_be_set(name)));
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
    host_home: &VerifiedPath,
    cwd: &VerifiedPath,
    mounts: &[(&str, Mount)],
    name: &str,
) -> anyhow::Result<VerifiedPath> {
    // The user can pick a symlink, for example to the current kernel build.
    let resolved = mount::host_path(raw, host_home, cwd)?;
    // The sandbox can write the project dir and the writable dir mounts,
    // and put symlinks there. The project share is not known here, so the
    // project dir counts as writable.
    let sources = mount::resolve_sources(mounts, host_home, cwd)?;
    let writable = mount::writable_dirs(Some(cwd), &sources);
    mount::check_stays_in(&messages::vm::custom_asset_kind(name), &resolved, &writable)?;
    match resolved.metadata() {
        Ok(_) => Ok(resolved),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            anyhow::bail!(messages::app::custom_asset_not_found(name, &resolved))
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
    use crate::util::VerifiedPath;

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

        let home = VerifiedPath::new_follow(&root, None).unwrap();
        let cwd = VerifiedPath::new_follow(&project, None).unwrap();
        let err = resolve_custom("Image", &home, &cwd, &[], "kernel")
            .unwrap_err()
            .to_string();
        assert!(err.contains("Set the real path"), "{err}");
        let real = kernel.to_str().unwrap();
        let resolved = resolve_custom(real, &home, &cwd, &[], "kernel").unwrap();
        assert_eq!(resolved.to_path().unwrap(), kernel);
    }
    /// Test that an asset write replaces the old file and leaves no temporary
    /// file, so that a reader never sees a partial asset.
    ///   1. Write an asset, then write it again with longer content
    ///   2. Check that the file has the new content
    ///   3. Check that the directory has only the asset file
    #[test]
    fn asset_write_replaces_file_and_leaves_no_temp_file() {
        use super::write_atomic;
        use crate::util::{Roots, VerifiedPath};

        let tmp = temp_dir();
        let dir = tmp.path();
        let roots = Roots::resolve(dir, dir, dir).unwrap();
        let verified = VerifiedPath::new(dir, &roots).unwrap();

        write_atomic(&verified, "Image", b"kernel-bytes", 0o644).unwrap();
        write_atomic(&verified, "Image", b"newer-and-longer-bytes", 0o644).unwrap();

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
