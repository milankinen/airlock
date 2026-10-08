//! Sweep-based garbage collector for the OCI cache.
//!
//! An image is considered live when some sandbox holds a hardlink to its
//! `images/<digest>` file (link count > 1). A layer is live when at least
//! one live image lists its digest. Everything else is deleted.
//!
//! Run this only after user-initiated removals (`Recreate`, `airlock rm`).
//! Running it on every `prepare()` would race with sibling sandboxes in
//! the middle of starting up — their hardlinks may not exist yet.

use std::collections::HashSet;
use std::os::unix::fs::MetadataExt;
use std::path::Path;

use crate::cache;

/// Minimal shape for parsing just the layer list from a cached image file —
/// avoids pulling in the full `OciImage` deserialization path here.
#[derive(serde::Deserialize)]
struct CachedLayers {
    #[serde(default)]
    image_layers: Vec<String>,
}

/// Remove every cached image file whose link count is 1 (no sandbox
/// references), then every layer dir not referenced by a surviving image.
/// Stray staging entries (`.download`, `.download.tmp`, `.tmp`) are always
/// removed — they're only meaningful mid-pull.
pub fn sweep() {
    sweep_images();
    let live = collect_live_layers();
    sweep_layers(&live);
}

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
            // Ignore leftover directories from the old layout (harmless) and
            // whatever else shows up; only files are real cache entries.
            continue;
        }
        if meta.nlink() <= 1 {
            let _ = std::fs::remove_file(&path);
        }
    }
}

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
        // `image_layers` holds versioned layer keys (e.g. `2.<hex>`) that
        // match the on-disk dir name 1:1 — no normalization needed.
        live.extend(parsed.image_layers);
    }
    live
}

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

#[allow(clippy::case_sensitive_file_extension_comparisons)]
fn is_staging_name(name: &str) -> bool {
    name.ends_with(".download.tmp") || name.ends_with(".download") || name.ends_with(".tmp")
}

fn remove_any(path: &Path) -> std::io::Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    }
}
