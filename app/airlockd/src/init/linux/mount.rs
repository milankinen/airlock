//! Thin wrappers around `mount(2)` that the rest of guest init builds on.
//! Each helper takes `&str` paths and reports failures with contextual
//! `anyhow` errors so callers can chain them without duplicating the
//! `CString` dance.

use tracing::debug;

/// Mount a VirtioFS share by its tag name at `/mnt/<tag>`.
pub(super) fn virtiofs(tag: &str) -> anyhow::Result<()> {
    let mount_point = format!("/mnt/{tag}");
    virtiofs_at(tag, &mount_point)
}

/// Mount a VirtioFS share by its tag name at an arbitrary path.
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

/// Create a bind mount using the `mount(2)` syscall directly.
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

/// Recursive bind mount (MS_BIND | MS_REC).
pub(super) fn bind_rec(src: &str, dst: &str) -> anyhow::Result<()> {
    let src_cstr = std::ffi::CString::new(src).unwrap();
    let dst_cstr = std::ffi::CString::new(dst).unwrap();
    let ret = unsafe {
        libc::mount(
            src_cstr.as_ptr(),
            dst_cstr.as_ptr(),
            std::ptr::null(),
            libc::MS_BIND | libc::MS_REC,
            std::ptr::null(),
        )
    };
    if ret != 0 {
        let err = std::io::Error::last_os_error();
        anyhow::bail!("failed to recursive bind-mount {src} → {dst}: {err}");
    }
    Ok(())
}

/// Mount a filesystem with optional data string.
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
    // Leak the CString to keep the pointer valid across the syscall
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
