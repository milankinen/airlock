//! Garbage collector for the OCI cache.
//!
//! Removes the cached images and layers that no sandbox uses.

use std::collections::HashSet;
use std::time::Duration;

use crate::cache;
use crate::data_dir::DataDir;
use crate::db::Db;
use crate::util::verified_path::VerifiedPath;

/// Minimal shape to parse only the layer list of a cached image file. Thus
/// this module does not need the full `OciImage` deserialization.
#[derive(serde::Deserialize)]
struct CachedLayers {
    #[serde(default)]
    image_layers: Vec<String>,
}

/// Remove the cached images and layers that no sandbox or template uses.
///
/// An image is in use if the image record of a sandbox or a template names
/// its digest. A layer is in use if at least one image in use lists it. The
/// sweep deletes all other images and layers. It also always removes stray
/// staging entries (`.download`, `.download.tmp`, `.tmp`), because they are
/// useful only during a pull.
///
/// Run this only after removals that the user started (`Recreate`,
/// `airlock rm`). If it runs on each `prepare()`, it races with sibling
/// sandboxes that are starting. Their layers can be on disk before their
/// image file is.
/// Args:
///  - `db`: The airlock database with the image records
///  - `data_dir`: Airlock data directory
pub async fn sweep(db: &Db, data_dir: &DataDir) {
    // Without the records, no image is known to be unused.
    let live = match super::record::live_digests(db, data_dir).await {
        Ok(live) => live,
        Err(e) => {
            tracing::warn!("gc: read image records: {e:#}");
            return;
        }
    };
    sweep_images(data_dir, &live);
    let live = collect_live_layers(data_dir);
    sweep_layers(data_dir, &live);
}

/// Age after which the sweep removes the temp file of an image write. A
/// younger temp file can belong to a write that runs now.
const STALE_TMP_AGE: Duration = Duration::from_hours(1);

/// Remove the cached image files whose digest is not in `live`, and old
/// temp files of image writes.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn sweep_images(data_dir: &DataDir, live: &HashSet<String>) {
    let Ok(images_root) = cache::images_root(data_dir) else {
        return;
    };
    let Ok(entries) = images_root.read_dir() else {
        return;
    };
    let live: HashSet<&str> = live.iter().map(|d| cache::digest_name(d)).collect();
    for entry in entries.flatten() {
        let Ok(path) = images_root.join(entry.file_name()) else {
            continue;
        };
        let Ok(meta) = path.symlink_metadata() else {
            continue;
        };
        if !meta.is_file() {
            // Ignore directories left from the old layout (they do no harm)
            // and other entries. Only files are real cache entries.
            continue;
        }
        let name = entry.file_name().to_string_lossy().into_owned();
        let stale = || {
            meta.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_TMP_AGE)
        };
        let unused = if name.ends_with(".tmp") {
            stale()
        } else {
            !live.contains(name.as_str())
        };
        if unused {
            let _ = path.remove_file();
        }
    }
}

/// Collect the layer keys of all remaining cached images.
fn collect_live_layers(data_dir: &DataDir) -> HashSet<String> {
    let mut live = HashSet::new();
    let Ok(images_root) = cache::images_root(data_dir) else {
        return live;
    };
    let Ok(entries) = images_root.read_dir() else {
        return live;
    };
    for entry in entries.flatten() {
        let Some(data) = images_root
            .join(entry.file_name())
            .ok()
            .and_then(|path| super::read_image_json(&path))
        else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<CachedLayers>(&data) else {
            continue;
        };
        // `image_layers` contains versioned layer keys (e.g. `2.<hex>`).
        // They are the same as the on-disk dir names, so no conversion is
        // necessary.
        live.extend(parsed.image_layers);
    }
    live
}

/// Remove staging entries and the layer dirs that are not in `live`.
fn sweep_layers(data_dir: &DataDir, live: &HashSet<String>) {
    let Ok(layers_root) = cache::layers_root(data_dir) else {
        return;
    };
    let Ok(entries) = layers_root.read_dir() else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(path) = layers_root.join(entry.file_name()) else {
            continue;
        };
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_staging_name(&name) {
            let _ = remove_any(&path);
            continue;
        }
        if !live.contains(name.as_str()) {
            let _ = path.remove_dir_all();
        }
    }
}

/// Return `true` if `name` is a staging entry of an unfinished pull.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_staging_name(name: &str) -> bool {
    name.ends_with(".download.tmp") || name.ends_with(".download") || name.ends_with(".tmp")
}

/// Remove a file or a directory tree.
fn remove_any(path: &VerifiedPath) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(m) if m.is_dir() => path.remove_dir_all(),
        Ok(_) => path.remove_file(),
        Err(e) => Err(e),
    }
}
