//! Garbage collector for the OCI cache.
//!
//! Removes the cached images and layers that no sandbox uses.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::time::Duration;

use crate::cache;
use crate::data_dir::DataDir;
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
/// An image is in use if a sandbox or a template has a hardlink to its `images/<digest>`
/// file (link count > 1). A layer is in use if at least one image in use
/// lists it. The sweep deletes all other images and layers. It also always
/// removes stray staging entries (`.download`, `.download.tmp`, `.tmp`),
/// because they are useful only during a pull.
///
/// Run this only after removals that the user started (`Recreate`,
/// `airlock rm`). If it runs on each `prepare()`, it races with sibling
/// sandboxes that are starting. Their hardlinks may not exist yet.
/// Args:
///  - `data_dir`: Airlock data directory
pub fn sweep(data_dir: &DataDir) {
    relink_templates(data_dir);
    sweep_images(data_dir);
    let live = collect_live_layers(data_dir);
    sweep_layers(data_dir, &live);
}

/// Make the image file of each template a hardlink to its cache entry
/// again.
// A pull of the same image under an other name writes the cache entry
// again, and the entry gets a new inode. A sandbox repairs its link on
// each start, but nothing starts a template. Without the repair, the sweep
// removes the image of the template.
fn relink_templates(data_dir: &DataDir) {
    let Ok(entries) = data_dir.templates.read_dir() else {
        return;
    };
    for entry in entries.flatten() {
        let Ok(link) = data_dir
            .templates
            .join(entry.file_name())
            .and_then(|t| t.join(crate::templates::IMAGE_FILE))
        else {
            continue;
        };
        let Some(image) = super::read_cached_image(&link) else {
            continue;
        };
        if let Err(e) = super::ensure_image_hardlink(data_dir, &link, &image) {
            tracing::warn!("gc: relink {link}: {e:#}");
        }
    }
}

/// Age after which the sweep removes the temp file of an image write. A
/// younger temp file can belong to a write that runs now.
const STALE_TMP_AGE: Duration = Duration::from_hours(1);

/// Remove the cached image files that have link count 1 (no sandbox uses
/// them), and old temp files of image writes.
fn sweep_images(data_dir: &DataDir) {
    let Ok(images_root) = cache::images_root(data_dir) else {
        return;
    };
    let Ok(entries) = images_root.read_dir() else {
        return;
    };
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
        let is_tmp = entry.file_name().to_string_lossy().ends_with(".tmp");
        let stale = || {
            meta.modified()
                .ok()
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age > STALE_TMP_AGE)
        };
        if meta.nlink() <= 1 && (!is_tmp || stale()) {
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
