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
        for d in parsed.image_layers {
            live.insert(cache::digest_name(&d).to_string());
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::HOME_LOCK;

    fn tempfile_dir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "airlock-gc-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    /// Write a cached image file with the given layer list.
    fn write_cached_image(digest: &str, layers: &[&str]) -> std::path::PathBuf {
        let path = cache::image_path(digest).unwrap();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        let json = serde_json::json!({
            "schema": "v1",
            "image_id": digest,
            "name": "test",
            "image_layers": layers,
            "container_home": "/root",
            "uid": 0,
            "gid": 0,
            "cmd": ["/bin/sh"],
            "env": [],
        });
        std::fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        path
    }

    /// Build a live image: write the cached file, then hardlink it so nlink > 1.
    fn make_live_image(digest: &str, layers: &[&str]) {
        let path = write_cached_image(digest, layers);
        let link = path.with_extension("sandbox-ref");
        std::fs::hard_link(&path, &link).unwrap();
    }

    fn make_orphan_image(digest: &str, layers: &[&str]) {
        write_cached_image(digest, layers);
    }

    fn make_layer(digest: &str) {
        let dir = cache::layer_dir(digest).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"x").unwrap();
    }

    #[test]
    fn sweep_keeps_live_images_and_their_layers() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        make_live_image("sha256:live", &["sha256:L1", "sha256:L2"]);
        make_layer("sha256:L1");
        make_layer("sha256:L2");

        sweep();

        assert!(cache::image_path("sha256:live").unwrap().exists());
        assert!(cache::layer_dir("sha256:L1").unwrap().exists());
        assert!(cache::layer_dir("sha256:L2").unwrap().exists());
    }

    #[test]
    fn sweep_removes_orphan_images_and_unshared_layers() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        make_orphan_image("sha256:orphan", &["sha256:X"]);
        make_layer("sha256:X");

        sweep();

        assert!(!cache::image_path("sha256:orphan").unwrap().exists());
        assert!(!cache::layer_dir("sha256:X").unwrap().exists());
    }

    #[test]
    fn sweep_keeps_layers_shared_with_live_image() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        make_live_image("sha256:keep", &["sha256:shared"]);
        make_orphan_image("sha256:drop", &["sha256:shared", "sha256:gone"]);
        make_layer("sha256:shared");
        make_layer("sha256:gone");

        sweep();

        assert!(cache::image_path("sha256:keep").unwrap().exists());
        assert!(!cache::image_path("sha256:drop").unwrap().exists());
        assert!(cache::layer_dir("sha256:shared").unwrap().exists());
        assert!(!cache::layer_dir("sha256:gone").unwrap().exists());
    }

    #[test]
    fn sweep_removes_stray_staging_entries() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let layers = cache::layers_root().unwrap();
        std::fs::write(layers.join("abc.download.tmp"), b"").unwrap();
        std::fs::write(layers.join("def.download"), b"").unwrap();
        std::fs::create_dir_all(layers.join("ghi.tmp")).unwrap();

        sweep();

        assert!(!layers.join("abc.download.tmp").exists());
        assert!(!layers.join("def.download").exists());
        assert!(!layers.join("ghi.tmp").exists());
    }
}
