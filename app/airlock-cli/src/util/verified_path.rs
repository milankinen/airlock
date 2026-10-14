//! Verified host paths.
//!
//! The guest can write some host directories and can put symlinks in them.
//! A verified path is a host path below a trusted root that has no symlink
//! below that root. Missing parts are allowed. The check runs again before
//! each use. Thus a later swap of a part for a symlink, or of a directory
//! for an other directory, is refused.
//!
//! Paths that the user picks (for example mount sources) can follow
//! symlinks one time when airlock resolves them. The result can be limited
//! to a directory.
//!
//! A verified path also gives the host file and Unix socket operations.
//! Each operation checks the path again just before it runs. Operations on
//! the entry itself (remove, rename, link metadata) accept any type at the
//! last name, because they do not follow it.

// This module wraps the plain path types. The rest of the crate uses only
// the verified paths.
#![allow(clippy::disallowed_types)]

use std::ffi::OsStr;
use std::fs::{self, DirBuilder, File, Metadata, OpenOptions, Permissions, ReadDir};
use std::hash::{Hash, Hasher};
use std::io::Read as _;
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Component, Path, PathBuf};
use std::{fmt, io};

use crate::messages;

/// Device and inode number of a file.
type Id = (u64, u64);

/// A host path that airlock checked for symlinks.
///
/// Keeps the path that the user wrote (for messages) and the absolute path
/// (for file operations). Equality and hash use the absolute path.
#[derive(Clone, Debug)]
pub(crate) struct VerifiedPath {
    given: PathBuf,
    resolved: PathBuf,
    // One item per component of `resolved`, from `/`. `None` for the
    // components that did not exist at the check. `to_path` compares the
    // directories that existed with their current identity. A directory
    // that is made after the check has no identity here. A swap of it for
    // an other real directory is not found. This window is accepted: a
    // symlink is still refused.
    ids: Vec<Option<Id>>,
}

/// The trusted root directories of the process. Relative paths and `~`
/// paths resolve from them.
#[derive(Clone, Debug)]
pub(crate) struct Roots {
    /// Home dir of the user.
    pub home: VerifiedPath,
    /// Current dir of the process.
    pub work: VerifiedPath,
    /// Airlock data directory.
    pub data: VerifiedPath,
}

/// The reason why airlock refuses a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// A component is a symlink.
    Symlink,
    /// A component before the last one is not a directory.
    NotDir,
    /// The last component is a FIFO.
    Fifo,
    /// A directory is not the same directory as at the check.
    Replaced,
    /// The path has a `..` component.
    Dot,
    /// A path that must be relative is absolute.
    Absolute,
    /// The resolved path is not below its allowed directory.
    Outside,
    /// The path is empty or has no file name.
    NoName,
}

/// Error of a refused path. It is the inner error of the `io::Error`.
#[derive(Debug)]
pub(crate) struct RefusedPath {
    /// The reason of the refusal.
    pub refusal: Refusal,
    given: PathBuf,
    resolved: PathBuf,
    at: PathBuf,
    scope: Option<PathBuf>,
}

impl fmt::Display for RefusedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        use messages::util as m;
        let outside;
        let reason = match self.refusal {
            Refusal::Symlink => m::REFUSED_SYMLINK,
            Refusal::NotDir => m::REFUSED_NOT_DIR,
            Refusal::Fifo => m::REFUSED_FIFO,
            Refusal::Replaced => m::REFUSED_REPLACED,
            Refusal::Dot => m::REFUSED_DOT,
            Refusal::Absolute => m::REFUSED_ABSOLUTE,
            Refusal::NoName => m::REFUSED_NO_NAME,
            Refusal::Outside => {
                outside =
                    m::refused_outside(self.scope.as_deref().unwrap_or(Path::new("/")).display());
                &outside
            }
        };
        f.write_str(&m::path_refused(
            self.given.display(),
            self.resolved.display(),
            self.at.display(),
            reason,
        ))
    }
}

impl std::error::Error for RefusedPath {}

/// Get the refusal reason of an error from this module.
/// Returns:
///   The reason, or `None` if `e` is not a refused path (for example a
///   normal I/O error).
pub(crate) fn refusal(e: &io::Error) -> Option<Refusal> {
    e.get_ref()
        .and_then(|inner| inner.downcast_ref::<RefusedPath>())
        .map(|r| r.refusal)
}

/// What the last component can be in a walk.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Last {
    /// All types, also a symlink. The operation does not follow it.
    Entry,
    /// All types except a symlink and a FIFO.
    Follow,
}

impl VerifiedPath {
    /// Check a path below the trusted roots. No component below the root
    /// can be a symlink. The last component can be of any type: the check
    /// before use refuses it if necessary. Missing components are allowed
    /// at all levels.
    /// Args:
    ///  - `path`: Path as the user wrote it. A `~` path resolves from the
    ///    home dir, a relative path from the current dir. An absolute path
    ///    resolves from the longest root that contains it, else from `/`.
    ///  - `roots`: Trusted roots of the process
    ///
    /// Returns:
    ///   The verified path, or error if a component before the last is a
    ///   symlink or not a directory, the path has a `..` component, or the
    ///   path is empty.
    pub(crate) fn new(path: impl AsRef<Path>, roots: &Roots) -> io::Result<Self> {
        let path = path.as_ref();
        no_empty(path)?;
        let (base, known, rel) = roots.split(path);
        let rel = lexical(path, rel)?;
        Self::verified(path.to_path_buf(), base.join(rel), known)
    }

    /// Resolve a path that the user picks and follow its symlinks. Use it
    /// only at entry points, for example dotfile config, mount sources,
    /// programs and a moved data dir.
    /// Args:
    ///  - `path`: Path to resolve. A relative path resolves from the current
    ///    dir of the process. Missing components at the end are allowed.
    ///  - `scope`: If set, the resolved path must be below this directory
    ///
    /// Returns:
    ///   The verified path, or error if the resolved path is outside
    ///   `scope` or a missing part has a `..` component.
    pub(crate) fn new_follow(
        path: impl AsRef<Path>,
        scope: Option<&VerifiedPath>,
    ) -> io::Result<Self> {
        let path = path.as_ref();
        Self::follow(path.to_path_buf(), path, scope)
    }

    /// Resolve the home dir of the user. It follows symlinks one time.
    /// Returns:
    ///   The home dir, or error if it is not known (for example no `HOME`).
    pub(crate) fn home_dir() -> io::Result<Self> {
        let home = dirs::home_dir()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, messages::app::HOME_NOT_SET))?;
        Self::new_follow(home, None)
    }

    /// Resolve the current dir of the process. It follows symlinks one
    /// time.
    /// Returns:
    ///   The current dir, or error if it is not known (for example because
    ///   it was deleted).
    pub(crate) fn current_dir() -> io::Result<Self> {
        let cwd = std::env::current_dir()
            .map_err(|e| io::Error::new(e.kind(), messages::cli::cannot_determine_cwd(&e)))?;
        Self::new_follow(cwd, None)
    }

    /// Check the path again and keep the identities of the directories
    /// that exist now. Use it after airlock made a directory of the path.
    /// Returns:
    ///   The checked path, or error as in [`Self::to_entry_path`].
    pub(crate) fn reverify(&self) -> io::Result<Self> {
        Self::verified(self.given.clone(), self.resolved.clone(), &self.ids)
    }

    /// Make a sub-path. The new components get the same checks as in
    /// [`Self::new`].
    /// Args:
    ///  - `rel`: Relative path without `..` components
    ///
    /// Returns:
    ///   The verified sub-path, or error if `rel` is empty or absolute, has
    ///   a `..` component, or has a symlink before the last component.
    pub(crate) fn join(&self, rel: impl AsRef<Path>) -> io::Result<Self> {
        let rel = rel.as_ref();
        no_empty(rel)?;
        let given = self.given.join(rel);
        let clean = lexical(&given, rel)?;
        Self::verified(given, self.resolved.join(clean), &self.ids)
    }

    /// Make a sub-path and follow its symlinks. Use it only at entry
    /// points, as [`Self::new_follow`].
    /// Args:
    ///  - `rel`: Path relative to this path. `..` components and an
    ///    absolute path are allowed. They resolve as in a shell.
    ///  - `scope`: If set, the resolved path must be below this directory
    ///
    /// Returns:
    ///   The verified path, or error if the resolved path is outside
    ///   `scope`.
    pub(crate) fn join_follow(
        &self,
        rel: impl AsRef<Path>,
        scope: Option<&VerifiedPath>,
    ) -> io::Result<Self> {
        let rel = rel.as_ref();
        Self::follow(self.given.join(rel), &self.resolved.join(rel), scope)
    }

    /// Get the parent path. It keeps the identities of the directories, so
    /// [`Self::to_path`] still finds a replaced directory.
    /// Returns:
    ///   The parent, or `None` at the root of the file system or for a
    ///   relative path with one component.
    pub(crate) fn parent(&self) -> Option<Self> {
        let resolved = self.resolved.parent()?;
        // Only an absolute path with a root has a resolved parent here.
        let given = self.given.parent().filter(|p| !p.as_os_str().is_empty());
        Some(Self {
            given: given.map_or_else(|| resolved.to_path_buf(), Path::to_path_buf),
            resolved: resolved.to_path_buf(),
            ids: self.ids[..resolved.components().count().min(self.ids.len())].to_vec(),
        })
    }

    /// Get the last component of the path.
    /// Returns:
    ///   The file name, or `None` for the root of the file system.
    pub(crate) fn file_name(&self) -> Option<&OsStr> {
        self.resolved.file_name()
    }

    /// Return `true` if `base` is this path or a parent of it. It compares
    /// the resolved paths as at the check and does not touch the file
    /// system.
    pub(crate) fn starts_with(&self, base: &VerifiedPath) -> bool {
        self.resolved.starts_with(&base.resolved)
    }

    /// Get the path as the user wrote it. Use it only for messages. Never
    /// use it for file operations: it is not checked.
    pub(crate) fn given(&self) -> &Path {
        &self.given
    }

    /// Check the path again before a file operation that follows the last
    /// component. Also the last component cannot be a symlink, so a create
    /// cannot follow a dangling link.
    /// Returns:
    ///   The absolute path, or error if a component is now a symlink, a
    ///   directory that existed at the check is now an other directory, or
    ///   the last component is a FIFO (an open of it can block).
    pub(crate) fn to_path(&self) -> io::Result<&Path> {
        self.walk(&self.ids, Last::Follow)?;
        Ok(&self.resolved)
    }

    /// Check the path again before a file operation that acts on the entry
    /// and does not follow it, for example remove or rename. The last
    /// component can be of any type, also a symlink.
    /// Returns:
    ///   The absolute path, or error as in [`Self::to_path`] for the
    ///   components before the last.
    pub(crate) fn to_entry_path(&self) -> io::Result<&Path> {
        self.walk(&self.ids, Last::Entry)?;
        Ok(&self.resolved)
    }

    /// Resolve `path` with symlinks followed, check the scope and make the
    /// verified path.
    fn follow(given: PathBuf, path: &Path, scope: Option<&VerifiedPath>) -> io::Result<Self> {
        let resolved = resolve_follow(&given, path)?;
        let known = match scope {
            Some(scope) => {
                if !resolved.starts_with(&scope.resolved) {
                    return Err(refused(
                        Refusal::Outside,
                        &given,
                        &resolved,
                        &resolved,
                        Some(&scope.resolved),
                    ));
                }
                scope.ids.as_slice()
            }
            None => &[],
        };
        Self::verified(given, resolved, known)
    }

    /// Check `resolved` and make the verified path. `known` are the
    /// identities of a parent path that must stay the same.
    fn verified(given: PathBuf, resolved: PathBuf, known: &[Option<Id>]) -> io::Result<Self> {
        let mut path = Self {
            given,
            resolved,
            ids: Vec::new(),
        };
        path.ids = path.walk(known, Last::Entry)?;
        Ok(path)
    }

    /// Check each component of the absolute path from `/`.
    /// Returns:
    ///   The current identities of the components.
    fn walk(&self, known: &[Option<Id>], last: Last) -> io::Result<Vec<Option<Id>>> {
        let count = self.resolved.components().count();
        let mut ids = Vec::with_capacity(count);
        let mut prefix = PathBuf::new();
        for (i, component) in self.resolved.components().enumerate() {
            prefix.push(component);
            let is_last = i + 1 == count;
            #[allow(clippy::disallowed_methods)]
            let md = match std::fs::symlink_metadata(&prefix) {
                Ok(md) => md,
                // From the first missing component on, the rest is lexical.
                Err(e) if e.kind() == io::ErrorKind::NotFound => break,
                Err(e) => return Err(e),
            };
            let kind = md.file_type();
            let refusal = if is_last {
                match last {
                    Last::Follow if kind.is_symlink() => Some(Refusal::Symlink),
                    Last::Follow if kind.is_fifo() => Some(Refusal::Fifo),
                    Last::Entry | Last::Follow => None,
                }
            } else if kind.is_symlink() {
                Some(Refusal::Symlink)
            } else if !kind.is_dir() {
                Some(Refusal::NotDir)
            } else {
                None
            };
            if let Some(refusal) = refusal {
                return Err(self.refuse(refusal, &prefix));
            }
            let id = (md.dev(), md.ino());
            // Only directories keep their identity. A file can get a new
            // inode from an atomic write of airlock.
            if kind.is_dir()
                && let Some(Some(old)) = known.get(i)
                && *old != id
            {
                return Err(self.refuse(Refusal::Replaced, &prefix));
            }
            ids.push(Some(id));
        }
        ids.resize(count, None);
        Ok(ids)
    }

    /// Make the error for a refusal at the component `at`.
    fn refuse(&self, refusal: Refusal, at: &Path) -> io::Error {
        refused(refusal, &self.given, &self.resolved, at, None)
    }
}

// The file operations on a verified path. Each one checks the path again
// and then runs the standard operation. The standard operations are only
// allowed here, after the check. Creates check the path one time and then
// run as the standard operation, so a small race window stays. Reads have
// no size limit: the caller sets it.
#[allow(clippy::disallowed_methods)]
impl VerifiedPath {
    /// Read the whole file.
    pub(crate) fn read(&self) -> io::Result<Vec<u8>> {
        fs::read(self.to_path()?)
    }

    /// Read the whole UTF-8 file.
    pub(crate) fn read_to_string(&self) -> io::Result<String> {
        fs::read_to_string(self.to_path()?)
    }

    /// Read the whole UTF-8 file, with a size limit.
    /// Args:
    ///  - `cap`: Maximum file size in bytes
    ///
    /// Returns:
    ///   The file content, or error if the read fails or the file is larger
    ///   than `cap`.
    pub(crate) fn read_to_string_capped(&self, cap: u64) -> io::Result<String> {
        let mut content = String::new();
        self.open()?
            .take(cap.saturating_add(1))
            .read_to_string(&mut content)?;
        if content.len() as u64 > cap {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                messages::util::file_too_large(self, cap),
            ));
        }
        Ok(content)
    }

    /// Write `contents` to the file. Creates or truncates the file, as the
    /// standard write. It is not atomic.
    // Only the asset extract uses it, and test builds do not extract.
    #[cfg_attr(test, allow(dead_code))]
    pub(crate) fn write(&self, contents: impl AsRef<[u8]>) -> io::Result<()> {
        fs::write(self.to_path()?, contents)
    }

    /// Open the file for reading.
    pub(crate) fn open(&self) -> io::Result<File> {
        File::open(self.to_path()?)
    }

    /// Create a new file and open it for writing. Error if the name exists.
    pub(crate) fn create_new(&self) -> io::Result<File> {
        File::create_new(self.to_path()?)
    }

    /// Open the file with the options `opts`.
    pub(crate) fn open_with(&self, opts: &OpenOptions) -> io::Result<File> {
        opts.open(self.to_path()?)
    }

    /// Get the metadata. Refuses a FIFO and a symlink at the last name.
    pub(crate) fn metadata(&self) -> io::Result<Metadata> {
        fs::metadata(self.to_path()?)
    }

    /// Get the metadata without a follow of the last component.
    pub(crate) fn symlink_metadata(&self) -> io::Result<Metadata> {
        fs::symlink_metadata(self.to_entry_path()?)
    }

    /// Return `true` if the path is a real directory (not a symlink).
    pub(crate) fn is_dir(&self) -> bool {
        self.metadata().is_ok_and(|m| m.is_dir())
    }

    /// Read the target of the symlink.
    pub(crate) fn read_link(&self) -> io::Result<PathBuf> {
        fs::read_link(self.to_entry_path()?)
    }

    /// List the entries of the directory.
    pub(crate) fn read_dir(&self) -> io::Result<ReadDir> {
        fs::read_dir(self.to_path()?)
    }

    /// Make the directory. Its parent must exist.
    pub(crate) fn mkdir(&self) -> io::Result<()> {
        fs::create_dir(self.to_path()?)
    }

    /// Make the directory and its missing parents. The check covers the
    /// existing components. The standard operation makes the missing ones.
    pub(crate) fn mkdirs(&self) -> io::Result<()> {
        fs::create_dir_all(self.to_path()?)
    }

    /// Make the directory with the options `builder`.
    pub(crate) fn mkdir_with(&self, builder: &DirBuilder) -> io::Result<()> {
        builder.create(self.to_path()?)
    }

    /// Remove the file. A symlink at the last name is removed, not
    /// followed.
    pub(crate) fn remove_file(&self) -> io::Result<()> {
        fs::remove_file(self.to_entry_path()?)
    }

    /// Remove the directory and all its content. A symlink at the last name
    /// is removed, not followed.
    pub(crate) fn remove_dir_all(&self) -> io::Result<()> {
        fs::remove_dir_all(self.to_entry_path()?)
    }

    /// Rename this entry to `to`. Replaces a file at `to`. A symlink at this
    /// path is moved, not followed.
    pub(crate) fn rename_to(&self, to: &VerifiedPath) -> io::Result<()> {
        fs::rename(self.to_entry_path()?, to.to_entry_path()?)
    }

    /// Set the permissions to `perm`.
    pub(crate) fn set_permissions(&self, perm: Permissions) -> io::Result<()> {
        fs::set_permissions(self.to_path()?, perm)
    }

    /// Make the hard link `link` to this file.
    pub(crate) fn hard_link_to(&self, link: &VerifiedPath) -> io::Result<()> {
        fs::hard_link(self.to_path()?, link.to_path()?)
    }

    /// Copy this file to `to`.
    /// Returns:
    ///   The number of bytes that were copied.
    pub(crate) fn copy_to(&self, to: &VerifiedPath) -> io::Result<u64> {
        fs::copy(self.to_path()?, to.to_path()?)
    }

    /// Connect to the Unix socket.
    pub(crate) fn connect_unix(&self) -> io::Result<std::os::unix::net::UnixStream> {
        std::os::unix::net::UnixStream::connect(self.to_path()?)
    }

    /// Bind an async Unix socket listener.
    pub(crate) fn bind_unix_async(&self) -> io::Result<tokio::net::UnixListener> {
        tokio::net::UnixListener::bind(self.to_path()?)
    }

    /// Connect to the Unix socket, async.
    pub(crate) async fn connect_unix_async(&self) -> io::Result<tokio::net::UnixStream> {
        tokio::net::UnixStream::connect(self.to_path()?).await
    }
}

impl Roots {
    /// Resolve the given roots for a test. Each root follows symlinks one
    /// time.
    /// Returns:
    ///   The roots, or error if a path cannot be resolved.
    #[cfg(test)]
    pub(crate) fn resolve(cwd: &Path, home: &Path, data_dir: &Path) -> io::Result<Self> {
        Ok(Self {
            work: VerifiedPath::new_follow(cwd, None)?,
            home: VerifiedPath::new_follow(home, None)?,
            data: VerifiedPath::new_follow(data_dir, None)?,
        })
    }

    /// Find the root of `path`.
    /// Returns:
    ///   The resolved base, the identities of its components and the rest
    ///   of `path` below the base.
    fn split<'a>(&'a self, path: &'a Path) -> (&'a Path, &'a [Option<Id>], &'a Path) {
        if let Ok(rest) = path.strip_prefix("~") {
            return (&self.home.resolved, &self.home.ids, rest);
        }
        if path.is_relative() {
            return (&self.work.resolved, &self.work.ids, path);
        }
        let mut best: Option<(usize, &VerifiedPath, &Path)> = None;
        for root in [&self.work, &self.home, &self.data] {
            // The user can write the root in its given form, for example
            // `/var/...` on macOS, or in its resolved form.
            for form in [&root.given, &root.resolved] {
                if let Ok(rest) = path.strip_prefix(form) {
                    let len = form.components().count();
                    if best.is_none_or(|(n, _, _)| len > n) {
                        best = Some((len, root, rest));
                    }
                }
            }
        }
        // A path outside all roots is checked from `/`. On macOS a path
        // through the `/tmp` or `/var` link is then refused: write the
        // resolved form or use a follow variant.
        match best {
            Some((_, root, rest)) => (&root.resolved, &root.ids, rest),
            None => (Path::new("/"), &[], path.strip_prefix("/").unwrap_or(path)),
        }
    }
}

impl fmt::Display for VerifiedPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.given.display().fmt(f)
    }
}

impl PartialEq for VerifiedPath {
    fn eq(&self, other: &Self) -> bool {
        self.resolved == other.resolved
    }
}

impl Eq for VerifiedPath {}

impl Hash for VerifiedPath {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.resolved.hash(state);
    }
}

/// Make a refusal error.
fn refused(
    refusal: Refusal,
    given: &Path,
    resolved: &Path,
    at: &Path,
    scope: Option<&Path>,
) -> io::Error {
    let kind = match refusal {
        Refusal::Dot | Refusal::Absolute | Refusal::NoName => io::ErrorKind::InvalidInput,
        _ => io::ErrorKind::Other,
    };
    io::Error::new(
        kind,
        RefusedPath {
            refusal,
            given: given.to_path_buf(),
            resolved: resolved.to_path_buf(),
            at: at.to_path_buf(),
            scope: scope.map(Path::to_path_buf),
        },
    )
}

/// Refuse an empty path. It names no file.
fn no_empty(path: &Path) -> io::Result<()> {
    if path.as_os_str().is_empty() {
        return Err(refused(Refusal::NoName, path, path, path, None));
    }
    Ok(())
}

/// Check that `rel` has only plain names. `.` components are allowed and
/// dropped.
/// Returns:
///   `rel` without `.` components, or error for `..` or an absolute path.
fn lexical(given: &Path, rel: &Path) -> io::Result<PathBuf> {
    let mut out = PathBuf::new();
    for component in rel.components() {
        match component {
            Component::Normal(name) => out.push(name),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(refused(Refusal::Dot, given, given, rel, None));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(refused(Refusal::Absolute, given, given, rel, None));
            }
        }
    }
    Ok(out)
}

/// Resolve `path` with symlinks followed. The missing components at the
/// end stay as they are.
fn resolve_follow(given: &Path, path: &Path) -> io::Result<PathBuf> {
    #[allow(clippy::disallowed_methods)]
    match std::fs::canonicalize(path) {
        Ok(resolved) => Ok(resolved),
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            // A path that ends in `..` has no file name. It cannot resolve
            // without its missing parent.
            let Some(name) = path.file_name() else {
                return Err(refused(Refusal::NoName, given, path, path, None));
            };
            let parent = match path.parent() {
                Some(p) if !p.as_os_str().is_empty() => p,
                _ => Path::new("."),
            };
            Ok(resolve_follow(given, parent)?.join(name))
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the symlink checks of verified paths.

    use std::os::unix::fs::symlink;

    use super::*;
    use crate::test_cfg::{TempDir, temp_dir};

    /// Roots that all point to a new temp dir.
    fn roots() -> (TempDir, Roots) {
        let dir = temp_dir();
        let roots = Roots::resolve(dir.path(), dir.path(), dir.path()).unwrap();
        (dir, roots)
    }

    /// The refusal of a result.
    fn refusal_of<T: fmt::Debug>(result: io::Result<T>) -> Refusal {
        refusal(&result.unwrap_err()).unwrap()
    }

    /// Test that a symlink in any component below the root is refused,
    /// because the guest can plant it to redirect host file operations.
    ///   1. Make a real dir and a symlink to it
    ///   2. Check that a path through the link is refused, also with `join`
    ///   3. Check that the link itself passes as an entry but not for use
    #[test]
    fn symlink_in_component_is_refused() {
        let (dir, roots) = roots();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        symlink("real", dir.path().join("link")).unwrap();

        assert_eq!(
            refusal_of(VerifiedPath::new("link/file", &roots)),
            Refusal::Symlink
        );
        let link = VerifiedPath::new("link", &roots).unwrap();
        assert!(link.to_entry_path().is_ok());
        assert_eq!(refusal_of(link.to_path()), Refusal::Symlink);
        assert_eq!(refusal_of(roots.work.join("link/x")), Refusal::Symlink);
        assert!(VerifiedPath::new("real/file", &roots).is_ok());
    }

    /// Test that missing components are allowed at all levels, so that
    /// airlock can name files and dirs before it makes them.
    ///   1. Check paths with the first, a middle and the last component
    ///      missing
    ///   2. Make the missing dirs and check that the paths still pass
    #[test]
    fn missing_components_at_any_level_are_allowed() {
        let (dir, roots) = roots();
        std::fs::create_dir(dir.path().join("a")).unwrap();
        let paths = [
            VerifiedPath::new("x/y/z", &roots).unwrap(),
            VerifiedPath::new("a/b/c", &roots).unwrap(),
            VerifiedPath::new("~/a/file", &roots).unwrap(),
        ];
        std::fs::create_dir_all(dir.path().join("x/y/z")).unwrap();
        std::fs::create_dir_all(dir.path().join("a/b/c")).unwrap();
        for path in &paths {
            assert!(path.to_path().is_ok(), "{path}");
        }
    }

    /// Test that `..` is refused in the checked forms and that `join`
    /// refuses an absolute path, because both can leave the root.
    ///   1. Check `new` and `join` with a `..` component
    ///   2. Check `join` with an absolute path
    #[test]
    fn parent_dir_and_absolute_join_are_refused() {
        let (_dir, roots) = roots();
        assert_eq!(
            refusal_of(VerifiedPath::new("a/../b", &roots)),
            Refusal::Dot
        );
        assert_eq!(refusal_of(roots.work.join("../x")), Refusal::Dot);
        assert_eq!(refusal_of(roots.work.join("/etc")), Refusal::Absolute);
    }

    /// Test that the check before use finds a directory that the guest
    /// swapped for a symlink after the first check.
    ///   1. Check `d/f` with a real dir `d`
    ///   2. Move `d` away and put a symlink to it at `d`
    ///   3. Check that `to_path` refuses the symlink
    #[test]
    fn to_path_detects_component_swapped_for_symlink() {
        let (dir, roots) = roots();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let path = VerifiedPath::new("d/f", &roots).unwrap();
        std::fs::rename(dir.path().join("d"), dir.path().join("d2")).unwrap();
        symlink("d2", dir.path().join("d")).unwrap();
        assert_eq!(refusal_of(path.to_path()), Refusal::Symlink);
    }

    /// Test that the check before use finds a directory that the guest
    /// replaced with an other real directory.
    ///   1. Check `d/f` with a real dir `d`
    ///   2. Move `d` away and make a new dir `d`
    ///   3. Check that `to_path` refuses the new dir
    #[test]
    fn to_path_detects_dir_replaced_by_other_dir() {
        let (dir, roots) = roots();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let path = VerifiedPath::new("d/f", &roots).unwrap();
        std::fs::rename(dir.path().join("d"), dir.path().join("d2")).unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        assert_eq!(refusal_of(path.to_path()), Refusal::Replaced);
    }

    /// Test that the check before use refuses a dangling symlink that the
    /// guest put at the last name after the first check. A create at that
    /// name would follow the link and make a file at the target.
    ///   1. Check the missing path `f`
    ///   2. Put a symlink at `f` to a missing file outside the root
    ///   3. Check that `to_path` refuses the symlink
    #[test]
    fn to_path_refuses_dangling_link_at_last_name() {
        let (dir, roots) = roots();
        let outside = temp_dir();
        let path = VerifiedPath::new("f", &roots).unwrap();
        symlink(outside.path().join("target"), dir.path().join("f")).unwrap();
        assert_eq!(refusal_of(path.to_path()), Refusal::Symlink);
    }

    /// Test that the check before use refuses a FIFO, because an open of a
    /// FIFO blocks until an other process opens its other end.
    ///   1. Make a FIFO and check its path
    ///   2. Check that `to_path` refuses it
    #[test]
    fn to_path_refuses_fifo() {
        let (dir, roots) = roots();
        let fifo = std::ffi::CString::new(
            dir.path()
                .join("fifo")
                .into_os_string()
                .into_encoded_bytes(),
        )
        .unwrap();
        // SAFETY: `fifo` is a valid C string.
        assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);
        let path = VerifiedPath::new("fifo", &roots).unwrap();
        assert_eq!(refusal_of(path.to_path()), Refusal::Fifo);
    }

    /// Test that the check before use accepts a socket. An open of a socket
    /// fails at once, and socket forwards need the path.
    ///   1. Make a listening socket and check its path
    ///   2. Check that `to_path` accepts it
    #[test]
    fn to_path_accepts_socket() {
        let (dir, roots) = roots();
        let _listener = std::os::unix::net::UnixListener::bind(dir.path().join("s.sock")).unwrap();
        let path = VerifiedPath::new("s.sock", &roots).unwrap();
        assert!(path.to_path().is_ok());
    }

    /// Test that `~` paths resolve from the home root and not from the
    /// current dir.
    ///   1. Make roots with a separate home dir
    ///   2. Check that `~` is the home dir and `~/x` is below it
    #[test]
    fn tilde_paths_resolve_from_home() {
        let cwd = temp_dir();
        let home = temp_dir();
        let roots = Roots::resolve(cwd.path(), home.path(), cwd.path()).unwrap();
        assert_eq!(&VerifiedPath::new("~", &roots).unwrap(), &roots.home);
        assert_eq!(
            VerifiedPath::new("~/x", &roots).unwrap(),
            roots.home.join("x").unwrap()
        );
    }

    /// Test that a trailing slash names the same path and that an empty
    /// path is refused, because it names no file.
    ///   1. Check that `a/` and `a` are equal
    ///   2. Check that `new` and `join` refuse an empty path
    #[test]
    fn trailing_slash_is_same_path_and_empty_path_is_refused() {
        let (_dir, roots) = roots();
        assert_eq!(
            VerifiedPath::new("a/", &roots).unwrap(),
            VerifiedPath::new("a", &roots).unwrap()
        );
        assert_eq!(refusal_of(VerifiedPath::new("", &roots)), Refusal::NoName);
        assert_eq!(refusal_of(roots.work.join("")), Refusal::NoName);
    }

    /// Test that an absolute path resolves from the longest root that
    /// contains it, by components and not by text. A wrong root would walk
    /// through the symlink in the root path and refuse it.
    ///   1. Make the home root through a link `b` and the data root
    ///      through a link `bc`
    ///   2. Check that paths below both link forms resolve to the right
    ///      real dirs
    #[test]
    fn absolute_path_uses_longest_root_by_component() {
        let dir = temp_dir();
        for real in ["real_b", "real_bc"] {
            std::fs::create_dir(dir.path().join(real)).unwrap();
        }
        symlink("real_b", dir.path().join("b")).unwrap();
        symlink("real_bc", dir.path().join("bc")).unwrap();
        let (b, bc) = (dir.path().join("b"), dir.path().join("bc"));
        let roots = Roots::resolve(dir.path(), &b, &bc).unwrap();

        let in_b = VerifiedPath::new(b.join("x"), &roots).unwrap();
        let in_bc = VerifiedPath::new(bc.join("x"), &roots).unwrap();
        assert!(in_b.to_path().unwrap().ends_with("real_b/x"));
        assert!(in_bc.to_path().unwrap().ends_with("real_bc/x"));
    }

    /// Test that an absolute path outside all roots is checked from `/`.
    /// A symlink in it is refused. On macOS this also refuses paths through
    /// the `/tmp` and `/var` links, so callers must give the resolved form.
    ///   1. Make a dir outside the roots with a symlink in it
    ///   2. Check that a path through the link is refused
    ///   3. On Linux, check that a plain path there passes
    #[test]
    fn absolute_path_outside_roots_is_checked_from_root() {
        let (_dir, roots) = roots();
        let other = temp_dir();
        let real = other.path().join("real");
        std::fs::create_dir(&real).unwrap();
        symlink(&real, other.path().join("link")).unwrap();
        assert_eq!(
            refusal_of(VerifiedPath::new(other.path().join("link/x"), &roots)),
            Refusal::Symlink
        );
        #[cfg(target_os = "linux")]
        assert!(VerifiedPath::new(real.join("x"), &roots).is_ok());
    }

    /// Test that `join` compares the dirs of its parent path with their
    /// identity at the first check, so a dir that the guest replaced
    /// before the join is found.
    ///   1. Check the dir `d`
    ///   2. Replace `d` with an other real dir
    ///   3. Check that `join` of a name below `d` refuses it
    #[test]
    fn join_detects_dir_replaced_after_parent_check() {
        let (dir, roots) = roots();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let d = VerifiedPath::new("d", &roots).unwrap();
        std::fs::rename(dir.path().join("d"), dir.path().join("d2")).unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        assert_eq!(refusal_of(d.join("f")), Refusal::Replaced);
    }

    /// Test that a dangling symlink in the middle of a followed path is
    /// refused. It cannot resolve, so the path keeps the link, and a later
    /// create would follow it.
    ///   1. Make a symlink `m` to a missing dir
    ///   2. Check that `new_follow` of `m/x` refuses the link
    #[test]
    fn new_follow_refuses_dangling_link_in_middle() {
        let (dir, _roots) = roots();
        symlink(dir.path().join("missing"), dir.path().join("m")).unwrap();
        assert_eq!(
            refusal_of(VerifiedPath::new_follow(dir.path().join("m/x"), None)),
            Refusal::Symlink
        );
    }

    /// Test that a followed path must stay in its scope, so that a link in
    /// the project cannot point a mount source to an other host dir.
    ///   1. Make a link to a dir in the scope and a link to a dir outside
    ///   2. Check that the link inside resolves and the link outside is
    ///      refused
    #[test]
    fn new_follow_outside_scope_is_refused() {
        let (dir, roots) = roots();
        let outside = temp_dir();
        std::fs::create_dir(dir.path().join("in")).unwrap();
        symlink("in", dir.path().join("to_in")).unwrap();
        symlink(outside.path(), dir.path().join("to_out")).unwrap();
        let scope = &roots.work;

        let inside = scope.join_follow("to_in", Some(scope)).unwrap();
        assert_eq!(inside, scope.join("in").unwrap());
        assert_eq!(
            refusal_of(scope.join_follow("to_out", Some(scope))),
            Refusal::Outside
        );
        assert_eq!(
            refusal_of(VerifiedPath::new_follow(
                dir.path().join("to_out/x"),
                Some(scope)
            )),
            Refusal::Outside
        );
    }

    /// Test that an absolute path below a root that the user wrote through
    /// a symlink resolves from the root. On macOS the temp dir is below the
    /// `/var` link, so this case is common.
    ///   1. Make a root through a symlink to a real dir
    ///   2. Check an absolute path in the link form of the root
    #[test]
    fn absolute_path_below_linked_root_is_allowed() {
        let dir = temp_dir();
        std::fs::create_dir(dir.path().join("real")).unwrap();
        symlink("real", dir.path().join("alias")).unwrap();
        let alias = dir.path().join("alias");
        let roots = Roots::resolve(&alias, &alias, &alias).unwrap();

        let path = VerifiedPath::new(alias.join("sub/file"), &roots).unwrap();
        assert_eq!(
            path.to_string(),
            alias.join("sub/file").display().to_string()
        );
        assert!(path.to_path().unwrap().ends_with("real/sub/file"));
    }

    /// Test that `parent` keeps the identities of the directories, so a
    /// replaced directory is still found, and that it stops at the root.
    ///   1. Check `d/f`, take its parent and replace `d`
    ///   2. Check that `to_path` of the parent refuses it
    ///   3. Check the name and that the parent of `/` is `None`
    #[test]
    fn parent_keeps_ids_and_stops_at_root() {
        let (dir, roots) = roots();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        let path = VerifiedPath::new("d/f", &roots).unwrap();
        let parent = path.parent().unwrap();
        assert_eq!(path.file_name().unwrap(), "f");
        assert_eq!(parent.file_name().unwrap(), "d");
        assert!(parent.to_path().is_ok());
        std::fs::rename(dir.path().join("d"), dir.path().join("d2")).unwrap();
        std::fs::create_dir(dir.path().join("d")).unwrap();
        assert_eq!(refusal_of(parent.to_path()), Refusal::Replaced);

        let root = VerifiedPath::new_follow("/", None).unwrap();
        assert!(root.parent().is_none());
        assert!(root.file_name().is_none());
    }

    /// A temp dir with a file `target` and a symlink `link` to it.
    fn dir_with_link() -> (TempDir, Roots) {
        let (dir, roots) = roots();
        std::fs::write(dir.path().join("target"), "data").unwrap();
        symlink("target", dir.path().join("link")).unwrap();
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
        VerifiedPath::new("link", &roots)
            .unwrap()
            .remove_file()
            .unwrap();
        assert!(std::fs::symlink_metadata(dir.path().join("link")).is_err());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("target")).unwrap(),
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
        from.rename_to(&to).unwrap();
        let md = std::fs::symlink_metadata(dir.path().join("moved")).unwrap();
        assert!(md.file_type().is_symlink());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("target")).unwrap(),
            "data"
        );
    }
}
