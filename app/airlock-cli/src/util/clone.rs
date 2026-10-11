//! Cheap file cloning.
//!
//! Clones large files, for example sparse VM disk images. Uses a
//! copy-on-write clone when the file system supports it. Otherwise copies
//! the file and keeps it sparse, so holes in the source never become data.

use std::fs::{File, OpenOptions};
use std::io;
use std::os::fd::AsRawFd;
use std::os::unix::fs::{FileExt, OpenOptionsExt, PermissionsExt};
use std::path::Path;

/// Size of the copy buffer.
const BUF_SIZE: usize = 1 << 20;

/// Granularity at which the copy skips all-zero data.
const ZERO_BLOCK: usize = 4096;

/// How `clone_file` created the copy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CloneKind {
    /// Copy-on-write clone. The file shares storage with the source.
    Reflink,
    /// Normal copy. Holes of the source stay holes if possible.
    Copy,
}

/// Clone the file `src` to the new file `dst`.
/// Args:
///  - `src`: Existing regular file
///  - `dst`: New file path. It must not exist. It gets mode 0600.
///
/// Returns:
///   How the clone was made, or error if `dst` exists or the copy fails.
///   After a failed copy `dst` does not exist.
pub(crate) fn clone_file(src: &Path, dst: &Path) -> io::Result<CloneKind> {
    #[cfg(target_os = "macos")]
    {
        match clonefile(src, dst) {
            Ok(()) => {
                if let Err(e) =
                    std::fs::set_permissions(dst, std::fs::Permissions::from_mode(0o600))
                {
                    let _ = std::fs::remove_file(dst);
                    return Err(e);
                }
                return Ok(CloneKind::Reflink);
            }
            // Other errors mean "no clone support here". Try a copy.
            Err(e)
                if matches!(
                    e.kind(),
                    io::ErrorKind::AlreadyExists | io::ErrorKind::NotFound
                ) =>
            {
                return Err(e);
            }
            Err(_) => {}
        }
    }

    let src_file = File::open(src)?;
    let dst_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(dst)?;

    #[cfg(target_os = "linux")]
    {
        // SAFETY: both fds are open and valid for the whole call.
        let rc = unsafe { libc::ioctl(dst_file.as_raw_fd(), libc::FICLONE, src_file.as_raw_fd()) };
        if rc == 0 {
            return Ok(CloneKind::Reflink);
        }
        // Any ioctl error leads to a normal copy. The copy reports real
        // errors, for example a read error, again.
    }

    match copy_sparse(&src_file, &dst_file) {
        Ok(()) => Ok(CloneKind::Copy),
        Err(e) => {
            drop(dst_file);
            let _ = std::fs::remove_file(dst);
            Err(e)
        }
    }
}

/// Create `dst` as an APFS clone of `src`.
#[cfg(target_os = "macos")]
fn clonefile(src: &Path, dst: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let to_c = |p: &Path| {
        CString::new(p.as_os_str().as_bytes())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))
    };
    let (src, dst) = (to_c(src)?, to_c(dst)?);
    // SAFETY: both pointers are valid NUL-terminated strings.
    if unsafe { libc::clonefile(src.as_ptr(), dst.as_ptr(), 0) } == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

/// Copy the content of `src` to `dst` and keep `dst` sparse.
fn copy_sparse(src: &File, dst: &File) -> io::Result<()> {
    let size = src.metadata()?.len();
    match copy_data_ranges(src, dst, size) {
        Ok(()) => {}
        Err(e) if e.raw_os_error() == Some(libc::EINVAL) => copy_skip_zeros(src, dst, size)?,
        Err(e) => return Err(e),
    }
    // Sets the size when the file ends with a hole.
    dst.set_len(size)
}

/// Seek `fd` to `offset` with `whence`.
/// Returns:
///   The new offset, `None` if there is no more data (`ENXIO`).
fn seek(fd: &File, offset: u64, whence: libc::c_int) -> io::Result<Option<u64>> {
    let offset =
        libc::off_t::try_from(offset).map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
    // SAFETY: the fd is open. `lseek` does not touch memory.
    let rc = unsafe { libc::lseek(fd.as_raw_fd(), offset, whence) };
    if rc >= 0 {
        return Ok(Some(rc as u64));
    }
    let err = io::Error::last_os_error();
    if err.raw_os_error() == Some(libc::ENXIO) {
        Ok(None)
    } else {
        Err(err)
    }
}

/// Copy the data ranges of `src`. Fails with `EINVAL` if the file
/// system does not support `SEEK_DATA`.
fn copy_data_ranges(src: &File, dst: &File, size: u64) -> io::Result<()> {
    let mut buf = vec![0u8; BUF_SIZE];
    let mut pos = 0;
    while pos < size {
        let Some(start) = seek(src, pos, libc::SEEK_DATA)? else {
            break;
        };
        // A hole always ends at the file end at the latest.
        let end = seek(src, start, libc::SEEK_HOLE)?.unwrap_or(size).min(size);
        copy_range(src, dst, start, end, &mut buf)?;
        pos = end;
    }
    Ok(())
}

/// Copy all of `src` but skip all-zero blocks, so holes stay.
fn copy_skip_zeros(src: &File, dst: &File, size: u64) -> io::Result<()> {
    let mut buf = vec![0u8; BUF_SIZE];
    copy_range(src, dst, 0, size, &mut buf)
}

/// Copy the bytes `start..end` from `src` to `dst` at the same offsets.
/// Blocks that contain only zeros are not written, because some file
/// systems report holes as data. Fails with `UnexpectedEof` if `src` ends
/// before `end`.
fn copy_range(src: &File, dst: &File, start: u64, end: u64, buf: &mut [u8]) -> io::Result<()> {
    let mut pos = start;
    while pos < end {
        let want = buf.len().min((end - pos) as usize);
        let n = match src.read_at(&mut buf[..want], pos) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        for (i, block) in buf[..n].chunks(ZERO_BLOCK).enumerate() {
            if block.iter().any(|b| *b != 0) {
                dst.write_all_at(block, pos + (i * ZERO_BLOCK) as u64)?;
            }
        }
        pos += n as u64;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    //! Tests for file cloning with sparse disk images.

    use std::os::unix::fs::MetadataExt;

    use super::*;
    use crate::test_cfg::temp_dir;

    const GIB: u64 = 1 << 30;

    /// Upper limit of allocated bytes for a sparse 1 GiB copy. A full copy
    /// allocates 1 GiB. Some file systems, for example APFS, allocate more
    /// space than the written blocks need.
    const MAX_ALLOCATED: u64 = 64 << 20;

    /// Create a sparse file of 1 GiB with data at offsets 0 and 512 MiB.
    fn sparse_source(path: &Path) {
        let f = File::create(path).unwrap();
        f.set_len(GIB).unwrap();
        f.write_all_at(&[0xab; 5000], 0).unwrap();
        f.write_all_at(&[0xcd; 7000], GIB / 2 + 100).unwrap();
    }

    /// Test that a disk image clone keeps content and size and stays
    /// sparse, because images are large and mostly empty.
    ///   1. Create a sparse 1 GiB file with data at two offsets
    ///   2. Clone it
    ///   3. Check size, data at both offsets and that few blocks are
    ///      allocated
    #[test]
    fn cloning_sparse_file_keeps_content_and_stays_sparse() {
        let dir = temp_dir();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        sparse_source(&src);

        clone_file(&src, &dst).unwrap();

        let meta = std::fs::metadata(&dst).unwrap();
        assert_eq!(meta.len(), GIB);
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        // Blocks are 512 bytes.
        assert!(
            meta.blocks() * 512 < MAX_ALLOCATED,
            "blocks: {}",
            meta.blocks()
        );
        let f = File::open(&dst).unwrap();
        let mut a = vec![0u8; 5000];
        f.read_exact_at(&mut a, 0).unwrap();
        assert!(a.iter().all(|b| *b == 0xab));
        let mut c = vec![0u8; 7000];
        f.read_exact_at(&mut c, GIB / 2 + 100).unwrap();
        assert!(c.iter().all(|b| *b == 0xcd));
        let mut z = [1u8; 100];
        f.read_exact_at(&mut z, GIB - 100).unwrap();
        assert_eq!(z, [0u8; 100]);
    }

    /// Test that the zero-skipping copy, used without `SEEK_DATA`, keeps
    /// the file sparse.
    ///   1. Create a sparse source
    ///   2. Copy it with the zero-skipping path
    ///   3. Check size, data and allocated blocks
    #[test]
    fn zero_skipping_copy_keeps_content_and_stays_sparse() {
        let dir = temp_dir();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        sparse_source(&src);
        let s = File::open(&src).unwrap();
        let d = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&dst)
            .unwrap();
        copy_skip_zeros(&s, &d, GIB).unwrap();
        d.set_len(GIB).unwrap();

        let meta = std::fs::metadata(&dst).unwrap();
        assert_eq!(meta.len(), GIB);
        assert!(
            meta.blocks() * 512 < MAX_ALLOCATED,
            "blocks: {}",
            meta.blocks()
        );
        let mut c = vec![0u8; 7000];
        d.read_exact_at(&mut c, GIB / 2 + 100).unwrap();
        assert!(c.iter().all(|b| *b == 0xcd));
    }

    /// Test that cloning never overwrites a file.
    ///   1. Create source and destination files
    ///   2. Clone to the existing destination
    ///   3. Check the error and that the destination is unchanged
    #[test]
    fn cloning_to_existing_file_is_error() {
        let dir = temp_dir();
        let (src, dst) = (dir.path().join("src"), dir.path().join("dst"));
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dst, b"old").unwrap();

        let err = clone_file(&src, &dst).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
    }
}
