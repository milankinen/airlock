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

use std::ffi::OsStr;
use std::hash::{Hash, Hasher};
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
    /// Airlock data directory.
    pub data_dir: VerifiedPath,
    // The current dir and home can be missing (deleted cwd, no `HOME`).
    // Only the paths that need them fail then.
    cwd: Result<VerifiedPath, RootError>,
    home: Result<VerifiedPath, RootError>,
}

/// A root that airlock cannot resolve. `io::Error` is not `Clone`, so
/// keep its parts.
#[derive(Clone, Debug)]
struct RootError {
    kind: io::ErrorKind,
    message: String,
}

impl RootError {
    fn new(e: &io::Error, message: String) -> Self {
        Self {
            kind: e.kind(),
            message,
        }
    }

    fn to_io(&self) -> io::Error {
        io::Error::new(self.kind, self.message.clone())
    }
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
    /// Check a path below the context roots. No component below the root
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
        let (base, known, rel) = roots.split(path)?;
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

impl Roots {
    /// Resolve the roots of the process: the current dir, `HOME` and
    /// `data_dir`. Each root follows symlinks one time. On macOS this
    /// resolves `/var` and `/tmp`, for example.
    /// Returns:
    ///   The roots, or error if `data_dir` cannot be resolved. A missing
    ///   current dir or home is not an error here. [`Self::cwd`] and
    ///   [`Self::home`] give the error later.
    pub(crate) fn for_process(data_dir: &Path) -> io::Result<Self> {
        let cwd = std::env::current_dir()
            .map_err(|e| RootError::new(&e, messages::cli::cannot_determine_cwd(&e)));
        let home = dirs::home_dir().ok_or_else(|| RootError {
            kind: io::ErrorKind::NotFound,
            message: messages::app::HOME_NOT_SET.to_string(),
        });
        Ok(Self {
            data_dir: VerifiedPath::new_follow(data_dir, None)?,
            cwd: cwd.and_then(|p| Self::root(&p)),
            home: home.and_then(|p| Self::root(&p)),
        })
    }

    /// Resolve the given roots for a test. Each root follows symlinks one
    /// time.
    /// Returns:
    ///   The roots, or error if a path cannot be resolved.
    #[cfg(test)]
    pub(crate) fn resolve(cwd: &Path, home: &Path, data_dir: &Path) -> io::Result<Self> {
        Ok(Self {
            cwd: Ok(VerifiedPath::new_follow(cwd, None)?),
            home: Ok(VerifiedPath::new_follow(home, None)?),
            data_dir: VerifiedPath::new_follow(data_dir, None)?,
        })
    }

    /// Get the current dir of the process.
    /// Returns:
    ///   The current dir, or error if it was not known at the start (for
    ///   example because it was deleted).
    pub(crate) fn cwd(&self) -> io::Result<&VerifiedPath> {
        self.cwd.as_ref().map_err(RootError::to_io)
    }

    /// Get the home dir of the user.
    /// Returns:
    ///   The home dir, or error if it is not known (for example no `HOME`).
    pub(crate) fn home(&self) -> io::Result<&VerifiedPath> {
        self.home.as_ref().map_err(RootError::to_io)
    }

    /// Resolve one root and keep the error for later.
    fn root(path: &Path) -> Result<VerifiedPath, RootError> {
        VerifiedPath::new_follow(path, None).map_err(|e| RootError::new(&e, e.to_string()))
    }

    /// Find the root of `path`.
    /// Returns:
    ///   The resolved base, the identities of its components and the rest
    ///   of `path` below the base. Error if `path` needs a missing root.
    fn split<'a>(&'a self, path: &'a Path) -> io::Result<(&'a Path, &'a [Option<Id>], &'a Path)> {
        if let Ok(rest) = path.strip_prefix("~") {
            let home = self.home()?;
            return Ok((&home.resolved, &home.ids, rest));
        }
        if path.is_relative() {
            let cwd = self.cwd()?;
            return Ok((&cwd.resolved, &cwd.ids, path));
        }
        let mut best: Option<(usize, &VerifiedPath, &Path)> = None;
        let roots = [
            self.cwd.as_ref().ok(),
            self.home.as_ref().ok(),
            Some(&self.data_dir),
        ];
        for root in roots.into_iter().flatten() {
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
        Ok(match best {
            Some((_, root, rest)) => (&root.resolved, &root.ids, rest),
            None => (Path::new("/"), &[], path.strip_prefix("/").unwrap_or(path)),
        })
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
        assert_eq!(
            refusal_of(roots.cwd().unwrap().join("link/x")),
            Refusal::Symlink
        );
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
        assert_eq!(refusal_of(roots.cwd().unwrap().join("../x")), Refusal::Dot);
        assert_eq!(
            refusal_of(roots.cwd().unwrap().join("/etc")),
            Refusal::Absolute
        );
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
        assert_eq!(
            &VerifiedPath::new("~", &roots).unwrap(),
            roots.home().unwrap()
        );
        assert_eq!(
            VerifiedPath::new("~/x", &roots).unwrap(),
            roots.home().unwrap().join("x").unwrap()
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
        assert_eq!(refusal_of(roots.cwd().unwrap().join("")), Refusal::NoName);
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
        let scope = roots.cwd().unwrap();

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
}
