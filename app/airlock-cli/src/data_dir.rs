//! Airlock data directory layout.
//!
//! All sandboxes share the user's airlock data directory. It contains the
//! database, the sandboxes that are not in their project, the templates,
//! the VM boot assets, the OCI images and layers, the host side of the pack
//! mounts, and fallback CLI sockets. The user settings can move the
//! directory, also with a link.
//!
//! The data directory is private: airlock makes it, the sandboxes directory
//! and the templates directory with access for the user only. All
//! sub-directories are made when airlock first needs them.

use std::os::unix::fs::DirBuilderExt as _;

use crate::messages;
use crate::util::VerifiedPath;

/// The airlock data directory and its sub-directories.
///
/// The paths of the sub-directories are fixed at the start. A
/// sub-directory possibly does not exist yet. Use [`make_dir`] or
/// [`create_private_dir`] before a write in it, and use the path that they
/// return.
#[derive(Clone, Debug)]
pub(crate) struct DataDir {
    /// The data directory.
    pub root: VerifiedPath,
    /// The database (`db/`).
    pub db: VerifiedPath,
    /// The sandboxes that are not in their project (`boxes/`). The user
    /// can move it to an other disk with a link.
    pub sandboxes: VerifiedPath,
    /// The templates (`templates/`).
    pub templates: VerifiedPath,
    /// The VM boot assets (`vm/`).
    // Only the asset extract uses it, and test builds do not extract.
    #[cfg_attr(test, allow(dead_code))]
    pub vm: VerifiedPath,
    /// The OCI image files (`oci/images/`).
    pub images: VerifiedPath,
    /// The OCI layer cache (`oci/layers/`).
    pub layers: VerifiedPath,
    /// The host side of the pack mounts (`share/all/packs/`).
    pub packs: VerifiedPath,
    /// The fallback CLI sockets (`sock/`).
    pub sock: VerifiedPath,
}

impl DataDir {
    /// Make the data directory if it does not exist, with mode 0700, and
    /// get its layout.
    /// Args:
    ///  - `root`: Data directory. It can be a link that the user made.
    ///
    /// Returns:
    ///   The layout, or error if airlock cannot make the directory or a
    ///   path check fails.
    pub(crate) fn init(root: &VerifiedPath) -> anyhow::Result<Self> {
        // The returned root has the identity of the new dir.
        let root = create_private_dir(root)?;
        Ok(Self {
            db: root.join(crate::db::DIR)?,
            sandboxes: sandboxes_dir(&root)?,
            templates: root.join(crate::templates::TEMPLATES_DIR)?,
            vm: root.join("vm")?,
            images: root.join("oci/images")?,
            layers: root.join("oci/layers")?,
            packs: root.join("share/all/packs")?,
            sock: root.join("sock")?,
            root,
        })
    }
}

/// Get the sandboxes directory of the data directory `root`.
fn sandboxes_dir(root: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    let dir = root.join(crate::sandboxes::BOXES_DIR)?;
    // Only airlock and the user can write the data dir. Thus a link here
    // is from the user, who moved the sandboxes to an other disk. Follow
    // only this link.
    if dir
        .symlink_metadata()
        .is_ok_and(|m| m.file_type().is_symlink())
    {
        return Ok(root.join_follow(crate::sandboxes::BOXES_DIR, None)?);
    }
    Ok(dir)
}

/// Make `dir` and its missing parents. A new directory gets mode 0700: the
/// data directory holds the database and the sandbox data.
/// Returns:
///   The directory, checked again after the create, or error if a part of
///   the path is a symlink or the directory cannot be made.
pub(crate) fn create_private_dir(dir: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    dir.mkdir_with(&builder)
        .map_err(|e| anyhow::anyhow!(messages::app::cannot_create_dir(dir, e)))?;
    Ok(dir.reverify()?)
}

/// Make the directory `dir` in the data directory, with its missing
/// parents.
/// Returns:
///   The directory, checked again after the create, or error if a part of
///   the path is a symlink or the directory cannot be made.
pub(crate) fn make_dir(dir: &VerifiedPath) -> anyhow::Result<VerifiedPath> {
    dir.mkdirs()
        .map_err(|e| anyhow::anyhow!(messages::app::cannot_create_dir(dir, e)))?;
    Ok(dir.reverify()?)
}
