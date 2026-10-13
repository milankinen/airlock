//! Host file and Unix socket operations on verified paths.
//!
//! Each operation checks its paths again just before it runs, and refuses
//! symlinks and swapped directories (see the verified path module).
//! Operations on the entry itself (remove, rename, link metadata) accept
//! any type at the last name, because they do not follow it. The
//! operations have the same names, arguments and results as the standard
//! file operations. They do not limit the size of reads. The caller sets
//! the limit. Creates check the path one time and then run as the
//! standard operation, so a small race window stays.

// The standard file operations are only allowed here, after the check.
#![allow(clippy::disallowed_methods, clippy::disallowed_types)]

use std::fs::{self, DirBuilder, File, Metadata, OpenOptions, Permissions, ReadDir};
use std::io;

use super::verified_path::VerifiedPath;

/// Read the whole file at `path`.
pub(crate) fn read(path: &VerifiedPath) -> io::Result<Vec<u8>> {
    fs::read(path.to_path()?)
}

/// Read the whole UTF-8 file at `path`.
pub(crate) fn read_to_string(path: &VerifiedPath) -> io::Result<String> {
    fs::read_to_string(path.to_path()?)
}

/// Write `contents` to the file at `path`. Creates or truncates the file,
/// as the standard write. It is not atomic.
// Only the asset extract uses it, and test builds do not extract.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn write(path: &VerifiedPath, contents: impl AsRef<[u8]>) -> io::Result<()> {
    fs::write(path.to_path()?, contents)
}

/// Open the file at `path` for reading.
pub(crate) fn open(path: &VerifiedPath) -> io::Result<File> {
    File::open(path.to_path()?)
}

/// Create a new file at `path` and open it for writing. Error if the
/// name exists.
pub(crate) fn create_new(path: &VerifiedPath) -> io::Result<File> {
    File::create_new(path.to_path()?)
}

/// Open the file at `path` with the options `opts`.
pub(crate) fn open_with(path: &VerifiedPath, opts: &OpenOptions) -> io::Result<File> {
    opts.open(path.to_path()?)
}

/// Get the metadata of `path`. Refuses a FIFO and a symlink at `path`.
pub(crate) fn metadata(path: &VerifiedPath) -> io::Result<Metadata> {
    fs::metadata(path.to_path()?)
}

/// Get the metadata of `path` without a follow of the last component.
pub(crate) fn symlink_metadata(path: &VerifiedPath) -> io::Result<Metadata> {
    fs::symlink_metadata(path.to_entry_path()?)
}

/// Read the target of the symlink `path`.
pub(crate) fn read_link(path: &VerifiedPath) -> io::Result<std::path::PathBuf> {
    fs::read_link(path.to_entry_path()?)
}

/// List the entries of the directory `path`.
pub(crate) fn read_dir(path: &VerifiedPath) -> io::Result<ReadDir> {
    fs::read_dir(path.to_path()?)
}

/// Make the directory `path`. Its parent must exist.
pub(crate) fn create_dir(path: &VerifiedPath) -> io::Result<()> {
    fs::create_dir(path.to_path()?)
}

/// Make the directory `path` and its missing parents. The check covers
/// the existing components. The standard operation makes the missing ones.
pub(crate) fn create_dir_all(path: &VerifiedPath) -> io::Result<()> {
    fs::create_dir_all(path.to_path()?)
}

/// Make the directory `path` with the options `builder`.
pub(crate) fn create_dir_with(path: &VerifiedPath, builder: &DirBuilder) -> io::Result<()> {
    builder.create(path.to_path()?)
}

/// Remove the file at `path`.
pub(crate) fn remove_file(path: &VerifiedPath) -> io::Result<()> {
    fs::remove_file(path.to_entry_path()?)
}

/// Remove the directory `path` and all its content. A symlink at `path`
/// is removed, not followed.
pub(crate) fn remove_dir_all(path: &VerifiedPath) -> io::Result<()> {
    fs::remove_dir_all(path.to_entry_path()?)
}

/// Rename `from` to `to`. Replaces a file at `to`. A symlink at `from`
/// is moved, not followed.
pub(crate) fn rename(from: &VerifiedPath, to: &VerifiedPath) -> io::Result<()> {
    fs::rename(from.to_entry_path()?, to.to_entry_path()?)
}

/// Set the permissions of `path` to `perm`.
pub(crate) fn set_permissions(path: &VerifiedPath, perm: Permissions) -> io::Result<()> {
    fs::set_permissions(path.to_path()?, perm)
}

/// Make the hard link `link` to the file `original`.
pub(crate) fn hard_link(original: &VerifiedPath, link: &VerifiedPath) -> io::Result<()> {
    fs::hard_link(original.to_path()?, link.to_path()?)
}

/// Copy the file `from` to `to`.
/// Returns:
///   The number of bytes that were copied.
pub(crate) fn copy(from: &VerifiedPath, to: &VerifiedPath) -> io::Result<u64> {
    fs::copy(from.to_path()?, to.to_path()?)
}

/// Connect to the Unix socket at `path`.
pub(crate) fn connect_unix(path: &VerifiedPath) -> io::Result<std::os::unix::net::UnixStream> {
    std::os::unix::net::UnixStream::connect(path.to_path()?)
}

/// Bind an async Unix socket listener at `path`.
pub(crate) fn bind_unix_async(path: &VerifiedPath) -> io::Result<tokio::net::UnixListener> {
    tokio::net::UnixListener::bind(path.to_path()?)
}

/// Connect to the Unix socket at `path`, async.
pub(crate) async fn connect_unix_async(path: &VerifiedPath) -> io::Result<tokio::net::UnixStream> {
    tokio::net::UnixStream::connect(path.to_path()?).await
}

#[cfg(test)]
mod tests {
    //! Tests for the operations on a symlink entry.

    use std::os::unix::fs::symlink;

    use super::*;
    use crate::test_cfg::{TempDir, temp_dir};
    use crate::util::Roots;

    /// A temp dir with a file `target` and a symlink `link` to it.
    fn dir_with_link() -> (TempDir, Roots) {
        let dir = temp_dir();
        fs::write(dir.path().join("target"), "data").unwrap();
        symlink("target", dir.path().join("link")).unwrap();
        let roots = Roots::resolve(dir.path(), dir.path(), dir.path()).unwrap();
        (dir, roots)
    }

    /// Test that the remove of a symlink that the guest planted removes
    /// only the link and not its target.
    ///   1. Make a file and a symlink to it
    ///   2. Remove the symlink
    ///   3. Check that the link is gone and the target stays
    #[test]
    fn remove_file_of_planted_symlink_removes_only_link() {
        let (dir, roots) = dir_with_link();
        remove_file(&VerifiedPath::new("link", &roots).unwrap()).unwrap();
        assert!(fs::symlink_metadata(dir.path().join("link")).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join("target")).unwrap(),
            "data"
        );
    }

    /// Test that the rename of a symlink moves the link and not its
    /// target.
    ///   1. Make a file and a symlink to it
    ///   2. Rename the symlink
    ///   3. Check that the new name is a symlink and the target stays
    #[test]
    fn rename_of_symlink_moves_link() {
        let (dir, roots) = dir_with_link();
        let from = VerifiedPath::new("link", &roots).unwrap();
        let to = VerifiedPath::new("moved", &roots).unwrap();
        rename(&from, &to).unwrap();
        let md = fs::symlink_metadata(dir.path().join("moved")).unwrap();
        assert!(md.file_type().is_symlink());
        assert_eq!(
            fs::read_to_string(dir.path().join("target")).unwrap(),
            "data"
        );
    }
}
