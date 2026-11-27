//! Image records of sandboxes and templates.
//!
//! Keeps the image of each sandbox and each template in the airlock
//! database: the image digest and the configured image name. Each owner has
//! its own record, also when owners share one cached image. The cache
//! cleanup keeps the images that the records name, and removes the records
//! of owners that are gone.
//!
//! Also takes the image files of older versions into the records.

use std::collections::HashSet;

use heed::Database;
use heed::types::{Bytes, Str};

use crate::data_dir::DataDir;
use crate::db::Db;
use crate::sandbox::is_valid_id;
use crate::util::verified_path::VerifiedPath;

/// Name of the image record database.
const DATABASE: &str = "images";

/// Record table: owner key → record JSON.
type Table = Database<Str, Bytes>;

/// Image file of a sandbox or a template of an older version. It was a hard
/// link to the cache entry, and the link count kept the image.
const LEGACY_IMAGE_FILE: &str = "image";

/// File in the sandbox directory with the image digest of an imported old
/// sandbox. Empty if the digest is not known. The next record read takes it
/// into the record.
pub(super) const IMPORT_MARKER: &str = "image.imported";

/// Largest image file of an older version that is read. Real files are a
/// few KB.
const MAX_LEGACY_IMAGE_FILE: u64 = 4 * 1024 * 1024;

/// Largest import marker that is read. A digest is about 71 bytes.
const MAX_IMPORT_MARKER: u64 = 256;

/// The image of a sandbox or a template.
#[derive(serde::Serialize, serde::Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct ImageRecord {
    /// Image digest. Empty if it is not known (an imported sandbox).
    pub digest: String,
    /// Configured image name. `None` if it is not known (an imported
    /// sandbox).
    pub name: Option<String>,
}

/// The owner of an image record.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ImageOwner {
    /// A sandbox in the data directory, by registry id.
    Sandbox(String),
    /// A stored template, by name.
    Template(String),
    /// The build sandbox of a template, by template name.
    TemplateBuild(String),
}

impl ImageOwner {
    /// Get the owner of the sandbox directory `sandbox_dir` in the data
    /// directory.
    /// Returns:
    ///   The owner, or error if the directory has no name.
    pub fn sandbox(sandbox_dir: &VerifiedPath) -> anyhow::Result<Self> {
        // The directory name is the registry id.
        let id = sandbox_dir
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| {
                anyhow::anyhow!(crate::messages::sandbox::not_sandbox_id(sandbox_dir))
            })?;
        Ok(Self::Sandbox(id.to_string()))
    }

    /// The database key of the owner.
    fn key(&self) -> String {
        match self {
            Self::Sandbox(id) => format!("sandbox/{id}"),
            Self::Template(name) => format!("template/{name}"),
            Self::TemplateBuild(name) => format!("template-build/{name}"),
        }
    }

    /// Get the owner of the database key `key`.
    /// Returns:
    ///   The owner, or `None` if the key is not valid.
    fn parse(key: &str) -> Option<Self> {
        let (kind, name) = key.split_once('/')?;
        match kind {
            "sandbox" if is_valid_id(name) => Some(Self::Sandbox(name.to_string())),
            "template" if crate::templates::is_valid_name(name) => {
                Some(Self::Template(name.to_string()))
            }
            "template-build" if crate::templates::is_valid_name(name) => {
                Some(Self::TemplateBuild(name.to_string()))
            }
            _ => None,
        }
    }

    /// Get the directory of the owner in `data_dir`.
    fn dir(&self, data_dir: &DataDir) -> anyhow::Result<VerifiedPath> {
        Ok(match self {
            Self::Sandbox(id) => data_dir.sandboxes.join(id)?,
            Self::Template(name) => data_dir.templates.join(name)?,
            Self::TemplateBuild(name) => {
                crate::templates::create::build_sandbox_dir(data_dir, name)?
            }
        })
    }
}

/// Get the image record of `owner`. If there is none, take the image files
/// of an older version in `dir` into a new record.
/// Args:
///  - `db`: The airlock database
///  - `owner`: Owner of the record
///  - `dir`: Directory of the owner
///
/// Returns:
///   The record, or `None` if the owner has no image yet.
pub(crate) async fn load(
    db: &Db,
    owner: &ImageOwner,
    dir: &VerifiedPath,
) -> anyhow::Result<Option<ImageRecord>> {
    if let Some(record) = get(db, owner).await? {
        return Ok(Some(record));
    }
    let Some(legacy) = read_legacy(dir) else {
        return Ok(None);
    };
    let table: Table = db.database(DATABASE).await?;
    let key = owner.key();
    let bytes = serde_json::to_vec(&legacy)?;
    // Another process can write the record after the read above. Its
    // record is newer, thus it stays.
    let record = db
        .write(move |txn| {
            if let Some(stored) = table.get(txn, &key)? {
                return Ok(parse(stored));
            }
            table.put(txn, &key, &bytes)?;
            Ok(Some(legacy))
        })
        .await?;
    remove_legacy(dir);
    Ok(record)
}

/// Get the image record of `owner`.
/// Returns:
///   The record, or `None` if there is none.
pub(crate) async fn get(db: &Db, owner: &ImageOwner) -> anyhow::Result<Option<ImageRecord>> {
    let key = owner.key();
    let record = db
        .read_existing(DATABASE, move |txn, table: Table| {
            Ok(table.get(txn, &key)?.and_then(parse))
        })
        .await?;
    Ok(record.flatten())
}

/// Write the image record `record` of `owner`.
pub(crate) async fn set(db: &Db, owner: &ImageOwner, record: ImageRecord) -> anyhow::Result<()> {
    let table: Table = db.database(DATABASE).await?;
    let key = owner.key();
    let bytes = serde_json::to_vec(&record)?;
    db.write(move |txn| Ok(table.put(txn, &key, &bytes)?)).await
}

/// Remove the image record of `owner`, if it has one.
pub(crate) async fn remove(db: &Db, owner: &ImageOwner) -> anyhow::Result<()> {
    let table: Table = db.database(DATABASE).await?;
    let key = owner.key();
    db.write(move |txn| Ok(table.delete(txn, &key).map(drop)?))
        .await
}

/// Give the image record of `from` to `to`. Does nothing if `from` has no
/// record.
pub(crate) async fn rename(db: &Db, from: &ImageOwner, to: &ImageOwner) -> anyhow::Result<()> {
    let table: Table = db.database(DATABASE).await?;
    let (from, to) = (from.key(), to.key());
    db.write(move |txn| {
        if let Some(bytes) = table.get(txn, &from)?.map(<[u8]>::to_vec) {
            table.put(txn, &to, &bytes)?;
            table.delete(txn, &from)?;
        }
        Ok(())
    })
    .await
}

/// Get the image digests that sandboxes and templates use. Takes the image
/// files of older versions into records first, and removes the records of
/// owners whose directory is gone.
/// Returns:
///   The digests, or error if the records cannot be read. Then nothing is
///   known to be unused.
pub(crate) async fn live_digests(db: &Db, data_dir: &DataDir) -> anyhow::Result<HashSet<String>> {
    take_legacy_records(db, data_dir).await?;
    let table: Table = db.database(DATABASE).await?;
    let data_dir = data_dir.clone();
    db.write(move |txn| {
        let mut live = HashSet::new();
        let mut gone = Vec::new();
        for item in table.iter(txn)? {
            let (key, bytes) = item?;
            let owner = ImageOwner::parse(key);
            if owner.as_ref().is_none_or(|owner| is_gone(owner, &data_dir)) {
                gone.push(key.to_string());
                continue;
            }
            // A record that does not parse keeps nothing. The next start
            // of its owner writes it again.
            if let Some(record) = parse(bytes) {
                live.insert(record.digest);
            }
        }
        for key in gone {
            table.delete(txn, &key)?;
        }
        Ok(live)
    })
    .await
}

/// Take the image files of older versions of all sandboxes and templates
/// into records. Thus a sandbox that did not start since the update keeps
/// its image.
async fn take_legacy_records(db: &Db, data_dir: &DataDir) -> anyhow::Result<()> {
    let sandboxes = owner_dirs(&data_dir.sandboxes, is_valid_id, ImageOwner::Sandbox);
    let templates = owner_dirs(
        &data_dir.templates,
        crate::templates::is_valid_name,
        ImageOwner::Template,
    );
    for (owner, dir) in sandboxes.into_iter().chain(templates) {
        load(db, &owner, &dir).await?;
    }
    Ok(())
}

/// Get the owners in the directory `root` that have an image file of an
/// older version.
/// Args:
///  - `root`: Sandboxes or templates directory
///  - `is_valid`: Name check of the owner kind
///  - `owner`: Makes the owner of a name
fn owner_dirs(
    root: &VerifiedPath,
    is_valid: fn(&str) -> bool,
    owner: fn(String) -> ImageOwner,
) -> Vec<(ImageOwner, VerifiedPath)> {
    let Ok(entries) = root.read_dir() else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            if !is_valid(&name) {
                return None;
            }
            let dir = root.join(&name).ok()?;
            let has_legacy = [LEGACY_IMAGE_FILE, IMPORT_MARKER]
                .iter()
                .any(|file| dir.join(file).is_ok_and(|p| p.symlink_metadata().is_ok()));
            has_legacy.then(|| (owner(name), dir))
        })
        .collect()
}

/// Check that the directory of `owner` does not exist. An error other than
/// a missing entry keeps the record.
fn is_gone(owner: &ImageOwner, data_dir: &DataDir) -> bool {
    owner.dir(data_dir).is_ok_and(|dir| {
        dir.symlink_metadata()
            .is_err_and(|e| e.kind() == std::io::ErrorKind::NotFound)
    })
}

/// Parse the record JSON `bytes`.
/// Returns:
///   The record, or `None` if the JSON is not valid.
fn parse(bytes: &[u8]) -> Option<ImageRecord> {
    serde_json::from_slice(bytes)
        .inspect_err(|e| tracing::warn!("image record: {e}"))
        .ok()
}

/// Read the image of an older version in the directory `dir`: the image
/// file, else the import marker.
/// Returns:
///   The record, or `None` if there is no such file.
fn read_legacy(dir: &VerifiedPath) -> Option<ImageRecord> {
    let image = dir
        .join(LEGACY_IMAGE_FILE)
        .ok()
        .and_then(|path| path.read_to_string_capped(MAX_LEGACY_IMAGE_FILE).ok())
        .and_then(|json| super::stored_image(json.as_bytes()));
    if image.is_some() {
        return image;
    }
    // An imported sandbox has only the digest.
    let digest = dir
        .join(IMPORT_MARKER)
        .ok()?
        .read_to_string_capped(MAX_IMPORT_MARKER)
        .ok()?;
    Some(ImageRecord {
        digest: digest.trim().to_string(),
        name: None,
    })
}

/// Remove the image files of an older version in the directory `dir`.
fn remove_legacy(dir: &VerifiedPath) {
    for file in [LEGACY_IMAGE_FILE, IMPORT_MARKER] {
        if let Ok(path) = dir.join(file) {
            let _ = path.remove_file();
        }
    }
}
