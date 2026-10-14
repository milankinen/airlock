//! Mount resolution.
//!
//! Converts the configured mounts to absolute host and guest paths. A mount
//! shares a directory or a single file from the host with the sandbox.
//!
//! Mount sources can be symlinks, for example into a dotfiles repo. The
//! symlinks are resolved one time, at the start. A source that goes into a
//! directory that the sandbox can write must stay in that directory: the
//! sandbox can change the symlinks there.

use std::ffi::OsString;
use std::io::{self, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::Components;

use crate::config::config_values::{ConfigValues, Mount};
use crate::messages;
use crate::util::VerifiedPath;
use crate::util::verified_path::{Refusal, refusal};

/// A mount with expanded and validated host and guest paths.
#[derive(Debug)]
pub struct ResolvedMount {
    /// Mount type: file or directory.
    pub mount_type: MountType,
    /// Config key of the mount, or `project` for the project mount.
    pub name: String,
    /// Source path as the config gives it, for example `./data`. For the
    /// project mount, the project directory.
    pub given: String,
    /// Source on the host, with its symlinks resolved at the start.
    pub source: VerifiedPath,
    /// Expanded absolute target path in the container.
    pub target: String,
    /// If `true`, the guest cannot write to the mount.
    pub read_only: bool,
}

/// Type of a mount: a directory (VirtioFS share) or a single file.
#[derive(Debug)]
pub enum MountType {
    /// A directory mount, shared to the guest as its own VirtioFS share.
    Dir {
        /// VirtioFS share tag: `project` for the project mount, and
        /// `dir_0`, `dir_1`, ... for user mounts.
        key: String,
    },
    /// A file mount. The file is hardlinked (or copied) into the sandbox
    /// overlay directory under `files/{rw|ro}/{mount_key}`. The `files/rw`
    /// and `files/ro` VirtioFS shares give these files to the guest. In the
    /// container, the target path is a symlink to
    /// `/airlock/.files/{rw|ro}/{mount_key}`.
    File {
        /// Config key of the mount.
        mount_key: String,
    },
}

impl ResolvedMount {
    /// VirtioFS share tag (for dir mounts) or config key (for file mounts).
    pub fn key(&self) -> &str {
        match &self.mount_type {
            MountType::Dir { key } => key.as_str(),
            MountType::File { mount_key } => mount_key.as_str(),
        }
    }

    /// Path of this mount in the VM, for debug output.
    pub fn vm_path(&self) -> String {
        match &self.mount_type {
            MountType::Dir { key } => format!("/mnt/{key}"),
            MountType::File { mount_key } => {
                let rw_or_ro = if self.read_only { "ro" } else { "rw" };
                format!("/airlock/.files/{rw_or_ro}/{mount_key}")
            }
        }
    }
}

/// Resolve the configured mounts to absolute paths and mount types.
///
/// Expands `~`, makes relative paths absolute, resolves symlinks, and
/// applies the `missing` action of each mount if its source does not exist.
/// Can create missing source directories and files. A create never follows
/// a symlink at the source.
/// Args:
///  - `mounts`: Enabled mounts as `(config key, mount)` pairs, sorted by key
///  - `host_home`: Host home for `~` expansion of source paths
///  - `container_home`: Guest home for `~` expansion of target paths
///  - `cwd`: Host project directory. Relative source paths resolve from it
///  - `guest_cwd`: Guest base directory for relative target paths
///  - `project_share`: If `true`, the sandbox can write the project
///    directory.
///
/// Returns:
///   The resolved mounts, without skipped ones. Error if a source is missing
///   and its action is `MissingAction::Fail`, if a create mode is not valid,
///   if the creation of a source fails, or if a source goes into a directory
///   that the sandbox can write and a symlink there points out of it.
pub fn resolve_mounts(
    mounts: &[(&str, Mount)],
    host_home: &VerifiedPath,
    container_home: &str,
    cwd: &VerifiedPath,
    guest_cwd: &str,
    project_share: bool,
) -> anyhow::Result<Vec<ResolvedMount>> {
    use crate::config::config_values::MissingAction;

    let found = resolve_sources(mounts, host_home, cwd)?;
    let writable = writable_dirs(project_share.then_some(cwd), &found);

    let mut result = Vec::new();
    let mut dir_idx: usize = 0;
    for Source {
        name,
        mount: m,
        resolved,
        is_dir,
    } in found
    {
        // Check before a create, so that a create cannot go through a link
        // out of the writable directory.
        check_stays_in(messages::vm::MOUNT_KIND_SOURCE, &resolved, &writable)?;
        let mut source = resolved;

        let is_dir = match is_dir {
            Some(is_dir) => is_dir,
            None => match m.missing {
                MissingAction::Fail => {
                    anyhow::bail!(messages::vm::mount_source_missing(&source));
                }
                MissingAction::Warn => {
                    crate::cli::log!(
                        "  {} {}",
                        crate::cli::bullet(),
                        messages::vm::mount_skipped_not_found(crate::cli::dim(&source.to_string()))
                    );
                    continue;
                }
                MissingAction::Ignore => continue,
                MissingAction::CreateDir => {
                    source.mkdirs()?;
                    let mode = parse_mode(m.create_mode.as_deref(), 0o755)?;
                    source.set_permissions(std::fs::Permissions::from_mode(mode))?;
                    // Pin the new dir before it becomes a share.
                    source = source.reverify()?;
                    true
                }
                MissingAction::CreateFile => {
                    let mode = parse_mode(m.create_mode.as_deref(), 0o644)?;
                    create_file(&source, m.file_content.as_deref().unwrap_or(""), mode)?;
                    // Pin the new file before it becomes a share.
                    source = source.reverify()?;
                    false
                }
            },
        };

        // Resolve relative target paths against guest_cwd (the same as
        // source paths against cwd).
        let target = join_str(guest_cwd, &expand_tilde_str(&m.target, container_home));

        // Dir mounts get numbered tags (dir_0, dir_1, ...) in config key
        // order. File mounts use the config key as their identifier.
        let mount_type = if is_dir {
            let key = format!("dir_{dir_idx}");
            dir_idx += 1;
            MountType::Dir { key }
        } else {
            MountType::File {
                mount_key: name.to_string(),
            }
        };

        result.push(ResolvedMount {
            mount_type,
            name: name.to_string(),
            given: m.source.clone(),
            source,
            target,
            read_only: m.read_only,
        });
    }

    Ok(result)
}

/// A mount source with its symlinks resolved. Its missing action did not
/// run yet.
pub(crate) struct Source<'a> {
    /// Config key of the mount.
    pub name: &'a str,
    /// The mount config.
    pub mount: &'a Mount,
    /// The source path with its symlinks resolved. Its given path is the
    /// absolute path before the symlinks are resolved.
    pub resolved: VerifiedPath,
    /// True for a directory, `None` if the source is missing.
    pub is_dir: Option<bool>,
}

/// Get the enabled mounts of `config`, sorted by config key.
pub(crate) fn enabled_mounts(config: &ConfigValues) -> Vec<(&str, Mount)> {
    let mut mounts: Vec<_> = config
        .mounts
        .iter()
        .filter(|(_, m)| m.enabled)
        .map(|(k, m)| (k.as_str(), m.clone()))
        .collect();
    mounts.sort_by_key(|(k, _)| *k);
    mounts
}

/// Resolve the sources of `mounts` and follow their symlinks. Creates
/// nothing. See [`resolve_mounts`] for the args.
/// Returns:
///   The sources, or error if a source cannot be resolved.
pub(crate) fn resolve_sources<'a>(
    mounts: &'a [(&'a str, Mount)],
    host_home: &VerifiedPath,
    cwd: &VerifiedPath,
) -> anyhow::Result<Vec<Source<'a>>> {
    let mut found = Vec::new();
    for (name, m) in mounts {
        let source = host_path(&m.source, host_home, cwd)?;
        let is_dir = match source.metadata() {
            Ok(md) => Some(md.is_dir()),
            // A dangling symlink is a missing source. A create refuses it.
            Err(e) if is_missing(&e) => None,
            Err(e) => return Err(e.into()),
        };
        found.push(Source {
            name,
            mount: m,
            resolved: source,
            is_dir,
        });
    }
    Ok(found)
}

/// Get the directories that the sandbox can write: the shared project
/// directory `project_dir` and the writable directory mounts of
/// `sources`. A missing source with `create-dir` counts as a directory.
pub(crate) fn writable_dirs(
    project_dir: Option<&VerifiedPath>,
    sources: &[Source],
) -> Vec<VerifiedPath> {
    use crate::config::config_values::MissingAction;
    let mut writable: Vec<_> = project_dir.into_iter().cloned().collect();
    for s in sources {
        let dir = s
            .is_dir
            .unwrap_or(matches!(s.mount.missing, MissingAction::CreateDir));
        if dir && !s.mount.read_only {
            writable.push(s.resolved.clone());
        }
    }
    writable
}

/// Resolve a host path from the config and follow its symlinks.
/// Args:
///  - `given`: Path from the config
///  - `host_home`: Host home. `~` expands from it
///  - `cwd`: Host project directory. A relative path resolves from it
///
/// Returns:
///   The resolved path. Its given path is the absolute path before the
///   symlinks are resolved. Error if the path cannot be resolved.
pub(crate) fn host_path(
    given: &str,
    host_home: &VerifiedPath,
    cwd: &VerifiedPath,
) -> io::Result<VerifiedPath> {
    if given == "~" {
        Ok(host_home.clone())
    } else if let Some(rest) = given.strip_prefix("~/") {
        host_home.join_follow(rest, None)
    } else if given.starts_with('/') {
        VerifiedPath::new_follow(given, None)
    } else {
        cwd.join_follow(given, None)
    }
}

/// Expand `~` in the path text `path` from the home directory `home`.
/// Used for guest paths, and for host paths that are only shown.
pub(crate) fn expand_tilde_str(path: &str, home: &str) -> String {
    if path == "~" {
        home.to_string()
    } else if let Some(rest) = path.strip_prefix("~/") {
        join_str(home, rest)
    } else {
        path.to_string()
    }
}

/// Join the path text `path` to the directory `base`. An absolute `path`
/// stays as it is.
pub(crate) fn join_str(base: &str, path: &str) -> String {
    if path.starts_with('/') || base.is_empty() {
        path.to_string()
    } else if base.ends_with('/') {
        format!("{base}{path}")
    } else {
        format!("{base}/{path}")
    }
}

/// Check if an error of a source probe means that the source is missing.
/// A dangling symlink at the last name also counts as missing.
fn is_missing(e: &io::Error) -> bool {
    e.kind() == io::ErrorKind::NotFound || refusal(e) == Some(Refusal::Symlink)
}

/// Create the missing source file `source` and its parent directories.
fn create_file(source: &VerifiedPath, content: &str, mode: u32) -> anyhow::Result<()> {
    // The resolved path has no symlinks: its parent is the resolved parent
    // plus the missing names.
    if let Some(parent) = source.parent() {
        parent.mkdirs()?;
    }
    // A new file never follows a symlink that appears at the name.
    let mut file = source.create_new()?;
    file.write_all(content.as_bytes())?;
    file.set_permissions(std::fs::Permissions::from_mode(mode))?;
    Ok(())
}

/// Check that a host path from the config stays in each directory that the
/// sandbox can write and that the path goes through. The sandbox can put
/// symlinks in these directories. Thus a path through such a symlink can
/// point to any host file.
/// Args:
///  - `kind`: Kind of the path for the error message, for example
///    `mount source`
///  - `resolved`: The path with its symlinks resolved. Its given path is
///    the absolute path before the symlinks are resolved
///  - `writable`: Directories that the sandbox can write
///
/// Returns:
///   Error if a place on the way of the given path (a directory, a symlink or the
///   end) is in a directory of `writable`, and `resolved` is not in the
///   deepest such directory. Also error if the given path is relative or a
///   symlink cannot be read.
///
/// The path must come from `new_follow`, `home_dir` or `current_dir` bases.
pub(crate) fn check_stays_in(
    kind: &str,
    resolved: &VerifiedPath,
    writable: &[VerifiedPath],
) -> anyhow::Result<()> {
    let target = resolved.to_entry_path()?;
    let dirs = writable
        .iter()
        .map(VerifiedPath::to_entry_path)
        .collect::<io::Result<Vec<_>>>()?;
    for place in visited_places(resolved)? {
        let place = place.to_entry_path()?;
        // A writable mount can be in the project. A symlink in the mount
        // must then stay in the mount.
        let Some(dir) = dirs
            .iter()
            .filter(|d| place.starts_with(d))
            .max_by_key(|d| d.components().count())
        else {
            continue;
        };
        anyhow::ensure!(
            target.starts_with(dir),
            messages::vm::path_leaves_writable_dir(kind, resolved, dir.display(), target.display())
        );
    }
    Ok(())
}

/// Most symlinks that one path can go through. Linux has the same limit.
const MAX_HOPS: usize = 40;

/// Resolve the given path of `path` one symlink at a time.
/// Returns:
///   Each place on the way: each directory, each symlink and the end.
///   Error if the given path is relative, a symlink cannot be read or there
///   are too many symlinks.
fn visited_places(path: &VerifiedPath) -> anyhow::Result<Vec<VerifiedPath>> {
    // The parts to resolve, the next part last. The target of a symlink
    // goes in front of the rest.
    fn parts(components: Components<'_>) -> Vec<OsString> {
        components.rev().map(|c| c.as_os_str().to_owned()).collect()
    }
    anyhow::ensure!(
        path.given().is_absolute(),
        messages::vm::path_not_absolute(path.given().display())
    );
    let root = VerifiedPath::new_follow("/", None)?;
    let mut todo = parts(path.given().components());
    let mut places = Vec::new();
    // `current` has no symlinks: each part in it was checked.
    let mut current = root.clone();
    let mut hops = 0;
    let mut missing = false;
    while let Some(part) = todo.pop() {
        // Any other part is a normal name.
        if part == "/" {
            current = root.clone();
        } else if part == ".." {
            current = current.parent().unwrap_or(current);
        } else if part != "." {
            let next = current.join(&part)?;
            // From the first missing part on, there are no symlinks.
            if !missing {
                match next.symlink_metadata() {
                    Ok(md) if md.file_type().is_symlink() => {
                        hops += 1;
                        if hops > MAX_HOPS {
                            return Err(io::Error::from_raw_os_error(libc::ELOOP).into());
                        }
                        todo.extend(parts(next.read_link()?.components()));
                        places.push(next);
                        continue;
                    }
                    Ok(_) => {}
                    Err(e) if e.kind() == io::ErrorKind::NotFound => missing = true,
                    Err(e) => return Err(e.into()),
                }
            }
            current = next;
            places.push(current.clone());
        }
    }
    Ok(places)
}

/// Parse an octal mode string (e.g. "755") into a `u32`. Return `default`
/// if there is no string.
fn parse_mode(s: Option<&str>, default: u32) -> anyhow::Result<u32> {
    match s {
        Some(s) => u32::from_str_radix(s, 8)
            .map_err(|_| anyhow::anyhow!(messages::vm::invalid_octal_mode(s))),
        None => Ok(default),
    }
}

/// Make sure that no directory mount exposes a protected host directory.
/// A mount exposes a directory if its source is the directory or one of
/// its parents, for example `/home/me`, `/home` or `/` for `/home/me`.
/// Args:
///  - `mounts`: Resolved mounts, with the project mount
///  - `protected`: `(name, path)` of each host directory that no mount can
///    expose, for example the home directory
///
/// Returns:
///   Error that names the first mount that exposes a protected directory.
pub fn check_exposure(
    mounts: &[ResolvedMount],
    protected: &[(&str, &VerifiedPath)],
) -> anyhow::Result<()> {
    // Compare the resolved paths, so that a symlink cannot hide the overlap.
    for m in mounts {
        let MountType::Dir { key } = &m.mount_type else {
            continue;
        };
        let source = m.source.to_path()?;
        let mut exposed = None;
        for (name, dir) in protected {
            if dir.to_path()?.starts_with(source) {
                exposed = Some((name, dir));
                break;
            }
        }
        let Some((name, dir)) = exposed else {
            continue;
        };
        let what = if key == "project" {
            messages::vm::MOUNT_KIND_PROJECT
        } else {
            messages::vm::MOUNT_KIND_SOURCE
        };
        anyhow::bail!(messages::vm::mount_exposes_directory(
            what, &m.source, name, dir
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    /// A directory mount of `source`. `key` is `project` for the project
    /// mount.
    fn dir(key: &str, source: &Path) -> ResolvedMount {
        ResolvedMount {
            mount_type: MountType::Dir { key: key.into() },
            name: key.into(),
            given: source.display().to_string(),
            source: VerifiedPath::new_follow(source, None).unwrap(),
            target: "/mnt/x".into(),
            read_only: true,
        }
    }

    /// Test that a directory mount of the home directory, the data
    /// directory or one of their parents is refused, because the sandbox
    /// then gets all secrets in them. Mounts below them stay allowed.
    ///   1. Make a home directory with a data directory and a pack mount
    ///      directory in it
    ///   2. Check that the home, its parents (also `/`), the data directory
    ///      and a symlink to the home are refused
    ///   3. Check that a project below the home, the pack mount directory
    ///      and a file mount of the home are allowed
    #[test]
    fn mount_of_home_data_dir_or_parent_is_refused_and_subdirectory_is_allowed() {
        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home/me");
        let data = home.join(".local/share/airlock");
        let pack = data.join("share/all/packs/claude");
        let project = home.join("src/app");
        std::fs::create_dir_all(&pack).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let link = tmp.path().join("link");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        let home_path = VerifiedPath::new_follow(&home, None).unwrap();
        let data_path = VerifiedPath::new_follow(&data, None).unwrap();
        let protected = [
            ("home directory", &home_path),
            ("airlock data directory", &data_path),
        ];

        for (key, source) in [
            ("project", home.as_path()),
            ("project", home.parent().unwrap()),
            ("dir_0", Path::new("/")),
            ("dir_0", data.as_path()),
            ("dir_0", link.as_path()),
        ] {
            let err = check_exposure(&[dir(key, source)], &protected)
                .expect_err(&source.display().to_string())
                .to_string();
            assert!(err.contains("insecure_mounts"), "{err}");
        }

        let file = ResolvedMount {
            mount_type: MountType::File {
                mount_key: "f".into(),
            },
            ..dir("f", &home)
        };
        let allowed = [dir("project", &project), dir("dir_0", &pack), file];
        check_exposure(&allowed, &protected).unwrap();
    }
}
