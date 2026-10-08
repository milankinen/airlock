//! Mount helpers for guest init.
//!
//! Small helpers that mount host shares, bind mounts and other filesystems,
//! with clear error messages.

use tracing::debug;

/// Mount a VirtioFS share by its tag name at `/mnt/<tag>`.
pub(super) fn virtiofs(tag: &str) -> anyhow::Result<()> {
    let mount_point = format!("/mnt/{tag}");
    virtiofs_at(tag, &mount_point)
}

/// Mount a VirtioFS share by its tag name at the given path. Creates the
/// mount point if necessary.
pub(super) fn virtiofs_at(tag: &str, mount_point: &str) -> anyhow::Result<()> {
    std::fs::create_dir_all(mount_point)?;
    let tag_cstr = std::ffi::CString::new(tag).unwrap();
    let mount_cstr = std::ffi::CString::new(mount_point).unwrap();
    let fstype = std::ffi::CString::new("virtiofs").unwrap();
    let ret = unsafe {
        libc::mount(
            tag_cstr.as_ptr(),
            mount_cstr.as_ptr(),
            fstype.as_ptr(),
            0,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to mount virtiofs {tag} at {mount_point}: {err}");
    }
    debug!("mounted virtiofs: {tag} → {mount_point}");
    Ok(())
}

/// Bind-mount `src` at `dst`, optionally read-only.
pub(super) fn bind(src: &str, dst: &str, read_only: bool) -> anyhow::Result<()> {
    let src_cstr = std::ffi::CString::new(src).unwrap();
    let dst_cstr = std::ffi::CString::new(dst).unwrap();
    let flags = if read_only {
        libc::MS_BIND | libc::MS_RDONLY
    } else {
        libc::MS_BIND
    };
    let ret = unsafe {
        libc::mount(
            src_cstr.as_ptr(),
            dst_cstr.as_ptr(),
            std::ptr::null(),
            flags,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to bind-mount {src} → {dst}: {err}");
    }
    Ok(())
}

/// Mount a filesystem.
/// Args:
///  - `source`: Mount source (device or name, for example `proc`)
///  - `target`: Mount point
///  - `fstype`: Filesystem type
///  - `flags`: `MS_*` mount flags
///  - `data`: Filesystem-specific options. Empty means no options.
pub(super) fn fs(
    source: &str,
    target: &str,
    fstype: &str,
    flags: libc::c_ulong,
    data: &str,
) -> anyhow::Result<()> {
    let src_cstr = std::ffi::CString::new(source).unwrap();
    let dst_cstr = std::ffi::CString::new(target).unwrap();
    let fs_cstr = std::ffi::CString::new(fstype).unwrap();
    // Leak the CString to keep the pointer valid during the syscall.
    let data_ptr = if data.is_empty() {
        std::ptr::null()
    } else {
        let c = std::ffi::CString::new(data).unwrap();
        let p = c.as_ptr().cast::<libc::c_void>();
        std::mem::forget(c);
        p
    };
    let ret = unsafe {
        libc::mount(
            src_cstr.as_ptr(),
            dst_cstr.as_ptr(),
            fs_cstr.as_ptr(),
            flags,
            data_ptr,
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to mount {fstype} at {target}: {err}");
    }
    debug!("mounted {fstype} at {target}");
    Ok(())
}
