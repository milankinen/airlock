//! Shared toolkit for the guest side of host bridges (clipboard, browser).
//!
//! A bridge exposes a host capability to container processes as an ordinary
//! program — a shell shim — that talks to airlockd through a FIFO. A shell
//! shim cannot open a unix socket without `nc`/`socat`, which minimal images
//! do not ship, whereas `cat > fifo` and `printf … > fifo` need nothing.
//!
//! Framing falls out of FIFO semantics: one open-to-EOF cycle is exactly one
//! operation. Opening a FIFO blocks — the read end waits for a writer and the
//! write end waits for a reader — so callers must run every open on the
//! blocking pool. Blocking inline would wedge the runtime of a process that
//! is PID 1.

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tracing::warn;

/// Container rootfs, matching `crate::net::host_socket_forward`. Writing here
/// lands in the overlayfs upper layer (or a tmpfs mounted over it, such as
/// `/run/airlock`), visible inside the container.
const ROOTFS: &str = "/mnt/overlay/rootfs";

/// Resolve a container path inside the rootfs, honouring chroot symlink
/// semantics via the shared helper.
pub fn in_rootfs(guest_path: &str) -> PathBuf {
    crate::util::resolve_in_root(Path::new(ROOTFS), guest_path)
}

/// Create a FIFO owned by the container user.
///
/// `mkfifo` is subject to umask, so the mode is set explicitly afterwards.
pub fn make_fifo(guest_path: &str, uid: u32, gid: u32) -> anyhow::Result<()> {
    make_fifo_at(&in_rootfs(guest_path), uid, gid)
}

/// [`make_fifo`] at a host path.
pub(crate) fn make_fifo_at(path: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A stale FIFO from a previous boot would still work, but recreating
    // keeps ownership and mode correct if the container user changed.
    let _ = std::fs::remove_file(path);

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // Safety: `c_path` is a valid NUL-terminated path for the duration of
    // the call. Mirrors the `libc::mknod` use in `crate::net::tun`.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if rc != 0 {
        return Err(anyhow::anyhow!(
            "mkfifo {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

    // Safety: chown on a path we just created.
    let rc = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
    if rc != 0 {
        warn!(
            "bridge: chown {} failed: {}",
            path.display(),
            std::io::Error::last_os_error()
        );
    }
    Ok(())
}

/// Write an executable (0755) shim script at `guest_path` in the rootfs,
/// creating parent directories as needed.
pub fn install_shim(guest_path: &str, body: &str) -> anyhow::Result<()> {
    let path = in_rootfs(guest_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, body)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Block until a writer opens the FIFO, then read until they close it,
/// retaining at most `limit` bytes.
///
/// Returns `Err(total)` when the writer sent more than `limit`. The FIFO is
/// still drained to EOF in that case — abandoning it early would leave the
/// writer blocked on a full pipe — but bytes past the limit are discarded
/// instead of buffered, so `cat /dev/zero > fifo` costs constant memory
/// rather than taking down PID 1.
pub fn read_capped(path: &Path, limit: u64) -> std::io::Result<Result<Vec<u8>, u64>> {
    let mut file = std::fs::File::open(path)?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut total: u64 = 0;

    loop {
        let n = file.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total <= limit {
            buf.extend_from_slice(&chunk[..n]);
        } else if !buf.is_empty() {
            // Over the limit: stop retaining and release what we held.
            buf = Vec::new();
        }
    }

    Ok(if total > limit { Err(total) } else { Ok(buf) })
}
