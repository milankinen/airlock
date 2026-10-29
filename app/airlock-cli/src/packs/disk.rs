//! Packs on the disk.
//!
//! Reads pack directories from the host disk and loads them with the
//! shared pack checks. A user pack directory holds one sub-directory for
//! each pack. A project pack is a directory in the project. Its path cannot
//! contain links and cannot go out of the project. Only the known pack
//! files are read, and other entries are ignored. A known file must be a
//! regular file, not a link, and its size is limited. Each pack gets a hash
//! of its files, so that a change of the pack is visible.

use std::collections::BTreeMap;
use std::io::Read;
use std::os::unix::ffi::OsStrExt;

use anyhow::{Context, bail, ensure};
use sha2::{Digest, Sha256};

use crate::packs::id::is_identifier;
use crate::packs::loader::{file_names, is_skipped, load_pack};
use crate::packs::snapshot::PackSnapshot;
use crate::packs::{PackId, PackRef, PackSource, PackVersionData};
use crate::util::VerifiedPath;

/// Maximum size of one pack file in bytes.
pub(super) const MAX_FILE_BYTES: u64 = super::snapshot::MAX_TEXT_BYTES as u64;
/// Maximum number of packs in one user pack directory.
pub(super) const MAX_PACKS_PER_DIR: usize = 64;

/// Load the user packs of all user pack directories.
/// Args:
///  - `dirs`: The directories. The flag is true if the user set the
///    directory in the settings.
///
/// Returns:
///   The packs sorted by name, and a warning for each missing directory
///   that the user set. An error if a pack is not valid, two directories
///   have a pack with the same name, or a directory cannot be read.
pub(super) fn load_user_packs(
    dirs: &[(VerifiedPath, bool)],
) -> anyhow::Result<(Vec<PackVersionData>, Vec<String>)> {
    let mut warnings = Vec::new();
    // The pack directories by name, from all directories.
    let mut pack_dirs: BTreeMap<String, VerifiedPath> = BTreeMap::new();
    for (dir, explicit) in dirs {
        let Some(found) = list_pack_dirs(dir, &mut warnings)? else {
            // The default directory is optional.
            if *explicit {
                warnings.push(crate::messages::packs::user_pack_dir_missing(dir));
            }
            continue;
        };
        for (name, pack_dir) in found {
            if let Some(first) = pack_dirs.get(&name) {
                bail!(crate::messages::packs::user_pack_duplicate(
                    PackId::local(&name),
                    first,
                    pack_dir
                ));
            }
            pack_dirs.insert(name, pack_dir);
        }
    }
    let packs = pack_dirs
        .into_iter()
        .map(|(name, pack_dir)| {
            let source = PackSource::Local {
                dir: pack_dir.clone(),
            };
            load_dir(&pack_dir, PackId::local(&name), source)
        })
        .collect::<anyhow::Result<_>>()?;
    Ok((packs, warnings))
}

/// List the pack directories of one user pack directory. Skips files (for
/// example a `README.md`), hidden entries and editor backups. Skips a link
/// and a directory whose name is not a pack name with a warning.
/// Args:
///  - `dir`: The user pack directory
///  - `warnings`: Receives the warnings
///
/// Returns:
///   The directories by name, or `None` if the directory does not exist. An
///   error if the directory cannot be read or has too many packs.
fn list_pack_dirs(
    dir: &VerifiedPath,
    warnings: &mut Vec<String>,
) -> anyhow::Result<Option<BTreeMap<String, VerifiedPath>>> {
    let entries = match dir.read_dir() {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context(crate::messages::packs::user_pack_dir_unreadable(dir)));
        }
    };
    let mut pack_dirs = BTreeMap::new();
    for entry in entries {
        let entry = entry.with_context(|| crate::messages::packs::user_pack_dir_unreadable(dir))?;
        let file_name = entry.file_name();
        let name = file_name.to_string_lossy();
        if is_skipped(&name) {
            continue;
        }
        let file_type = entry
            .file_type()
            .with_context(|| crate::messages::packs::user_pack_dir_unreadable(dir))?;
        let shown = format!("{dir}/{name}");
        // A link is skipped, also a link to a directory: the files of the pack
        // must be below the directory that the user picked. The warning
        // tells the user why the pack is not there.
        if file_type.is_symlink() {
            warnings.push(crate::messages::packs::user_pack_link_skipped(&shown));
            continue;
        }
        if !file_type.is_dir() {
            continue;
        }
        if file_name.to_str().is_none_or(|name| !is_identifier(name)) {
            warnings.push(crate::messages::packs::user_pack_name_skipped(&shown));
            continue;
        }
        let pack_dir = dir.join(&*name)?;
        pack_dirs.insert(name.into_owned(), pack_dir);
        ensure!(
            pack_dirs.len() <= MAX_PACKS_PER_DIR,
            crate::messages::packs::user_packs_too_many(dir, MAX_PACKS_PER_DIR)
        );
    }
    Ok(Some(pack_dirs))
}

/// Find and load the project pack `id` in the project `root`.
/// Args:
///  - `root`: Project root directory
///  - `id`: Pack id. It must be a project pack id (`./path`).
///
/// Returns:
///   The pack, or an error if the directory does not exist, its path has a
///   link, or the pack is not valid (see [`load_dir`]).
pub(super) fn load_project_pack(
    root: &VerifiedPath,
    id: PackId,
) -> anyhow::Result<PackVersionData> {
    let PackRef::Project { path } = id.pack_ref() else {
        bail!(crate::messages::packs::not_project_pack(&id));
    };
    // Join one name at a time. Then each name is the last one at its join,
    // and the next join refuses it if it is a link. The path is in the
    // normal form, so it has no `..`.
    let mut dir = root.clone();
    for name in path.split('/') {
        dir = dir
            .join(name)
            .with_context(|| crate::messages::packs::project_pack_unreadable(&id))?;
    }
    // The last name must be a real directory, not a link.
    match dir.symlink_metadata() {
        Ok(meta) if meta.is_dir() => {}
        // The messages show the path as the config has it (`./path`).
        Ok(_) => bail!(crate::messages::packs::project_pack_not_dir(&id)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            bail!(crate::messages::packs::project_pack_missing(&id))
        }
        Err(e) => {
            return Err(anyhow::Error::from(e)
                .context(crate::messages::packs::project_pack_unreadable(&id)));
        }
    }
    let root_path = root
        .to_entry_path()
        .with_context(|| crate::messages::packs::project_pack_unreadable(&id))?;
    let source = PackSource::Project {
        dir: dir.clone(),
        dir_name: project_dir_name(root_path.as_os_str().as_bytes(), path),
    };
    load_dir(&dir, id, source)
}

/// Make the mount directory name of a project pack.
/// Args:
///  - `root`: Resolved path of the project root
///  - `path`: Pack path relative to the project root, with no `./`
///
/// Returns:
///   `project.<hash>`: the first 16 hex digits of the SHA-256 of the root
///   and the path. A NUL separates them, because a path cannot contain a
///   NUL.
fn project_dir_name(root: &[u8], path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(root);
    hasher.update([0]);
    hasher.update(path.as_bytes());
    let hash = hex::encode(hasher.finalize());
    format!("project.{}", &hash[..16])
}

/// Read and load the pack directory `dir`.
/// Args:
///  - `dir`: The pack directory
///  - `id`: Pack id
///  - `source`: Where the pack comes from
///
/// Returns:
///   The pack with its source, content hash and snapshot, or an error if
///   a file is not valid (see [`read_files`] and [`load_pack`]).
fn load_dir(dir: &VerifiedPath, id: PackId, source: PackSource) -> anyhow::Result<PackVersionData> {
    let at = dir.to_string();
    let files = read_files(dir)?;
    let by_name: BTreeMap<&str, &[u8]> = files
        .iter()
        .map(|(name, bytes)| (name.as_str(), bytes.as_slice()))
        .collect();
    let mut data = load_pack(&at, id, None, &by_name)?;
    data.metadata.source = source;
    // The hash and the snapshot come from the same bytes that the loader
    // parsed. The install fingerprint uses them, and no file is read again
    // later.
    data.content_hash = Some(content_hash(&files));
    data.snapshot = Some(PackSnapshot::new(
        data.metadata.id.as_str(),
        files
            .iter()
            .map(|(name, bytes)| (name.as_str(), bytes.as_slice())),
    ));
    Ok(data)
}

/// Read the known files of a pack directory. Ignores all other entries.
/// Returns:
///   The contents by file name, or an error if a known file is a link or
///   not a regular file, or is larger than the limit.
// Read only the known names. The guest can write the project, and a
// directory listing would let it make airlock read any number of files.
fn read_files(dir: &VerifiedPath) -> anyhow::Result<BTreeMap<String, Vec<u8>>> {
    let mut files = BTreeMap::new();
    for name in file_names() {
        let path = dir.join(&name)?;
        let meta = match path.symlink_metadata() {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(e.into()),
        };
        ensure!(
            meta.file_type().is_file(),
            crate::messages::packs::pack_file_not_regular(&path)
        );
        files.insert(name, read_capped(&path)?);
    }
    Ok(files)
}

/// Read a pack file with the size limit [`MAX_FILE_BYTES`].
fn read_capped(path: &VerifiedPath) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    path.open()?
        .take(MAX_FILE_BYTES + 1)
        .read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= MAX_FILE_BYTES,
        crate::messages::packs::pack_file_too_large(path, MAX_FILE_BYTES)
    );
    Ok(bytes)
}

/// Compute the SHA-256 of the files of a pack. Each file adds its name
/// length, name, content length and content, in name order. The lengths
/// make the input not ambiguous.
fn content_hash(files: &BTreeMap<String, Vec<u8>>) -> [u8; 32] {
    let mut hasher = Sha256::new();
    for (name, bytes) in files {
        hasher.update((name.len() as u64).to_le_bytes());
        hasher.update(name.as_bytes());
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(bytes);
    }
    hasher.finalize().into()
}
