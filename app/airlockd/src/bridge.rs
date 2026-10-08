//! Common parts of host bridges.
//!
//! A bridge gives container processes access to a host capability, for
//! example the clipboard or the browser. Container processes use an ordinary
//! program in the container, so they do not need to know about the bridge.
//! The bridge needs no extra tools in the container image.

use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use tracing::warn;

/// Container rootfs, same as in `crate::net::host_socket_forward`. Files
/// written here go to the overlayfs upper layer (or to a tmpfs mounted on
/// it, such as `/run/airlock`). The container sees them.
const ROOTFS: &str = "/mnt/overlay/rootfs";

/// Resolve a container path inside the container rootfs, with chroot
/// symlink semantics (see [`crate::util::resolve_in_root`]).
pub fn in_rootfs(guest_path: &str) -> PathBuf {
    crate::util::resolve_in_root(Path::new(ROOTFS), guest_path)
}

/// Create a FIFO with mode 0600, owned by the container user.
///
/// Bridge shims use FIFOs because a shell shim cannot open a unix socket
/// without `nc`/`socat`, and minimal images do not include them.
/// `cat > fifo` and `printf … > fifo` need no extra tools.
///
/// IMPORTANT: Opening a FIFO blocks. The read end waits for a writer and the
/// write end waits for a reader. Callers must do every open on the blocking
/// pool. An open on the runtime thread would stop the runtime of PID 1.
///
/// Args:
///  - `guest_path`: FIFO path as the container sees it
///  - `uid`, `gid`: Container user and group that own the FIFO
///
/// Returns:
///   Error if the FIFO cannot be created. A failed `chown` only logs a
///   warning.
pub fn make_fifo(guest_path: &str, uid: u32, gid: u32) -> anyhow::Result<()> {
    make_fifo_at(&in_rootfs(guest_path), uid, gid)
}

/// [`make_fifo`] at a host path.
pub(crate) fn make_fifo_at(path: &Path, uid: u32, gid: u32) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // A FIFO from a previous boot would still work. But a new FIFO has the
    // correct owner and mode if the container user changed.
    let _ = std::fs::remove_file(path);

    let c_path = std::ffi::CString::new(path.as_os_str().as_bytes())?;
    // Safety: `c_path` is a valid NUL-terminated path for the duration of
    // the call. Same as the `libc::mknod` use in `crate::net::tun`.
    let rc = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
    if rc != 0 {
        return Err(anyhow::anyhow!(
            "mkfifo {}: {}",
            path.display(),
            std::io::Error::last_os_error()
        ));
    }
    // `mkfifo` applies the umask, so set the mode explicitly.
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;

    // Safety: `c_path` is the valid path of the FIFO that this function made.
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

/// Write an executable (0755) shim script into the container rootfs.
/// Creates the parent directories if necessary.
/// Args:
///  - `guest_path`: Script path as the container sees it
///  - `body`: Script contents
pub fn install_shim(guest_path: &str, body: &str) -> anyhow::Result<()> {
    let path = in_rootfs(guest_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, body)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
    Ok(())
}

/// Read one payload from a FIFO, with a size limit.
///
/// Blocks until a writer opens the FIFO, then reads until the writer closes
/// it. Always reads to EOF, also when the payload is too large. One
/// open-to-EOF cycle of a FIFO is exactly one bridge operation. Call this
/// function on the blocking pool (see [`make_fifo`]).
/// Args:
///  - `path`: FIFO path on the guest
///  - `limit`: Maximum payload size in bytes
///
/// Returns:
///   `Ok(Ok(payload))` if the payload fits in `limit`. `Ok(Err(total))` with
///   the total size if the writer sent more than `limit`.
pub fn read_capped(path: &Path, limit: u64) -> std::io::Result<Result<Vec<u8>, u64>> {
    // Read to EOF also after the limit. If the read stops early, the writer
    // stays blocked on a full pipe. The loop discards the bytes after the
    // limit, so `cat /dev/zero > fifo` uses constant memory and cannot stop
    // PID 1.
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
            // Over the limit: free the kept bytes and keep no more.
            buf = Vec::new();
        }
    }

    Ok(if total > limit { Err(total) } else { Ok(buf) })
}
