//! Garbage collector for the OCI cache.
//!
//! Removes the cached images and layers that no sandbox uses.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::cache;

/// Minimal shape to parse only the layer list of a cached image file. Thus
/// this module does not need the full `OciImage` deserialization.
#[derive(serde::Deserialize)]
struct CachedLayers {
    #[serde(default)]
    image_layers: Vec<String>,
}

/// Remove the cached images and layers that no sandbox uses.
///
/// An image is in use if a sandbox has a hardlink to its `images/<digest>`
/// file (link count > 1). A layer is in use if at least one image in use
/// lists it. The sweep deletes all other images and layers. It also always
/// removes stray staging entries (`.download`, `.download.tmp`, `.tmp`),
/// because they are useful only during a pull.
///
/// Run this only after removals that the user started (`Recreate`,
/// `airlock rm`). If it runs on each `prepare()`, it races with sibling
/// sandboxes that are starting. Their hardlinks may not exist yet.
pub fn sweep() {
    sweep_images();
    let live = collect_live_layers();
    sweep_layers(&live);
}

/// Remove the cached image files that have link count 1 (no sandbox uses
/// them).
fn sweep_images() {
    let Ok(images_root) = cache::images_root() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&images_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::metadata(&path) else {
            continue;
        };
        if !meta.is_file() {
            // Ignore directories left from the old layout (they do no harm)
            // and other entries. Only files are real cache entries.
            continue;
        }
        if meta.nlink() <= 1 {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Collect the layer keys of all remaining cached images.
fn collect_live_layers() -> HashSet<String> {
    let mut live = HashSet::new();
    let Ok(images_root) = cache::images_root() else {
        return live;
    };
    let Ok(entries) = std::fs::read_dir(&images_root) else {
        return live;
    };
    for entry in entries.flatten() {
        let Ok(data) = std::fs::read(entry.path()) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_slice::<CachedLayers>(&data) else {
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
fn sweep_layers(live: &HashSet<String>) {
    let Ok(layers_root) = cache::layers_root() else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&layers_root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_staging_name(&name) {
            let _ = remove_any(&path);
            continue;
        }
        if !live.contains(name.as_str()) {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// Return `true` if `name` is a staging entry of an unfinished pull.
#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_staging_name(name: &str) -> bool {
    name.ends_with(".download.tmp") || name.ends_with(".download") || name.ends_with(".tmp")
}

/// Remove a file or a directory tree.
fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    }
}
