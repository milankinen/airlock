//! Embedded VM assets (kernel, initramfs, hypervisor binaries).
//!
//! These files are compiled into the `airlock` binary via `include_bytes!`. On
//! first run (or after a build changes the checksum), they are extracted to
//! `~/.cache/airlock/vm/` so the hypervisor can memory-map them.
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

        let dir = crate::cache::cache_dir()?.join("vm");
        std::fs::create_dir_all(&dir)?;

        // Serialize the checksum-check-and-extract across processes: take a
        // blocking exclusive lock before reading the checksum so two concurrent
        // first-runs (e.g. right after an upgrade) can't both rewrite the boot
        // assets, and so a booting hypervisor never memory-maps a file another
        // process is mid-rewrite on. Released when `_lock` drops.
        let _lock = acquire_extract_lock(&dir.join("lock"))?;

        let checksum_file = dir.join("checksum");
        let cached_checksum = std::fs::read_to_string(&checksum_file).unwrap_or_default();
        if cached_checksum.trim() != CHECKSUM {
            #[cfg(not(feature = "distroless"))]
            {
                // Write via temp file + rename so a reader (or a second process)
                // never observes Image/initramfs truncated mid-write.
                write_atomic(&dir, "Image", include_bytes!("../../../target/vm/Image"))?;
                write_atomic(
                    &dir,
                    "initramfs.gz",
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

/// Write `data` to `dir/name` via a sibling temp file + rename, so a reader
/// never observes the destination truncated mid-write and a concurrent process
/// can't boot from a half-written file. Cross-platform (unlike
/// [`write_executable`], no executable bit is set — used for `Image` /
/// `initramfs.gz`). The temp name carries the pid so a stray temp left by
/// another process can never be renamed into place.
#[cfg(not(feature = "distroless"))]
fn write_atomic(dir: &std::path::Path, name: &str, data: &[u8]) -> anyhow::Result<()> {
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    std::fs::write(&tmp, data)?;
    std::fs::rename(&tmp, dir.join(name))?;
    Ok(())
}

/// Acquire a blocking exclusive advisory lock on `path`, held until the
/// returned handle drops. Serializes the checksum-check-and-extract in
/// [`Assets::init`] across processes. Blocking (not fail-fast) because
/// extraction is brief: a second process simply waits, then observes the
/// freshly written checksum and skips re-extracting. Mirrors the `flock`
/// pattern in `project::acquire_lock` / `vault::acquire_file_lock`.
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

#[cfg(all(test, not(feature = "distroless")))]
mod tests {
    use super::write_atomic;

    #[test]
    fn write_atomic_replaces_file_and_leaves_no_tmp() {
        let dir = std::env::temp_dir().join(format!(
            "airlock-assets-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();

        write_atomic(&dir, "Image", b"kernel-bytes").unwrap();
        assert_eq!(std::fs::read(dir.join("Image")).unwrap(), b"kernel-bytes");

        // Overwriting an existing asset is atomic and clean.
        write_atomic(&dir, "Image", b"newer-and-longer-bytes").unwrap();
        assert_eq!(
            std::fs::read(dir.join("Image")).unwrap(),
            b"newer-and-longer-bytes"
        );

        // No leftover ".tmp" sibling after either write.
        let leftover: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(leftover.is_empty(), "temp file left behind: {leftover:?}");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
