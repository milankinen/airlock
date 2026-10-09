//! Tests for the extraction of layer tarballs into the layer cache:
//! whiteouts, reuse, interrupted downloads, races and path escapes.

use super::*;
use crate::test_cfg::home::TempHome;

/// The value of the extended attribute `name` of `path`.
fn xattr_of(path: &Path, name: &str) -> Option<Vec<u8>> {
    xattr::get(path, name).unwrap()
}

/// The names of staging entries (`.tmp` and `.download`) in the layer
/// cache.
fn staging_entries() -> Vec<String> {
    std::fs::read_dir(cache::layers_root().unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext == "tmp" || ext == "download")
        })
        .collect()
}

/// Test that a layer extracts with its whiteouts as overlayfs xattrs, and
/// that a cached layer is not fetched again.
///   1. Cache a layer with a file whiteout and an opaque directory marker
///   2. Check the regular files
///   3. Check that the whiteout is an empty file with the whiteout xattr
///   4. Check the opaque xattrs and that no staging entry is left
///   5. Ask for the layer again and check that no fetch happens
#[test]
fn layer_tarball_extracts_with_whiteouts_as_overlay_xattrs_and_is_reused() {
    let _home = TempHome::new();
    let tar = LayerTar::default()
        .dir("etc")
        .file("etc/hello", "world")
        .file("etc/.wh.gone", "")
        .file("bin/sh", "#!/bin/sh\n")
        .file("opt/app/.wh..wh..opq", "")
        .file("opt/app/new", "n")
        .gz();
    let key = cache_layer("sha256:whiteouts", &tar);
    let layer = cache::layer_dir(&key).unwrap();

    assert_eq!(std::fs::read(layer.join("etc/hello")).unwrap(), b"world");
    assert!(layer.join("bin/sh").is_file());
    assert!(layer.join("opt/app/new").is_file());
    let gone = layer.join("etc/gone");
    assert_eq!(std::fs::metadata(&gone).unwrap().len(), 0);
    assert_eq!(
        xattr_of(&gone, "user.overlay.whiteout").as_deref(),
        Some(&b"y"[..])
    );
    // A directory with a whiteout gets opaque "x", so overlayfs looks for
    // whiteouts in it. The `.wh..wh..opq` marker makes a directory fully
    // opaque with "y".
    assert_eq!(
        xattr_of(&layer.join("etc"), "user.overlay.opaque").as_deref(),
        Some(&b"x"[..])
    );
    assert_eq!(
        xattr_of(&layer.join("opt/app"), "user.overlay.opaque").as_deref(),
        Some(&b"y"[..])
    );
    assert!(staging_entries().is_empty());

    let again = layer::ensure_layer_cached(
        "sha256:whiteouts",
        |_| panic!("cached layer must not be fetched again"),
        None,
    )
    .unwrap();
    assert_eq!(again, layer);
}

/// Test that a partial download from an earlier run is discarded and the
/// layer is fetched again.
///   1. Write a partial download file for the layer
///   2. Cache the layer
///   3. Check the layer contents and that no staging entry is left
#[test]
fn interrupted_download_is_discarded_and_fetched_again() {
    let _home = TempHome::new();
    let key = cache::layer_key("sha256:interrupted");
    let stale = cache::layers_root()
        .unwrap()
        .join(format!("{key}.download.tmp"));
    std::fs::write(&stale, b"partial garbage").unwrap();

    cache_layer(
        "sha256:interrupted",
        &LayerTar::default().file("ok", "yes").gz(),
    );

    assert!(cache::layer_dir(&key).unwrap().join("ok").is_file());
    assert!(staging_entries().is_empty());
}

/// Test that a layer that another process publishes during the download is
/// kept, and the own download is discarded.
///   1. Start to cache a layer with a fetch that also publishes the layer
///      directory, as a peer process does
///   2. Check that the peer layer stays and the own content is absent
///   3. Check that no staging entry is left
#[test]
fn layer_published_by_peer_during_download_is_kept() {
    let _home = TempHome::new();
    let key = cache::layer_key("sha256:race");
    let layer = cache::layer_dir(&key).unwrap();
    let tar = LayerTar::default().file("loser", "x").gz();

    layer::ensure_layer_cached(
        "sha256:race",
        |dest| {
            std::fs::create_dir_all(&layer)?;
            std::fs::write(layer.join("winner"), b"kept")?;
            Ok(std::fs::write(dest, &tar)?)
        },
        None,
    )
    .unwrap();

    assert_eq!(std::fs::read(layer.join("winner")).unwrap(), b"kept");
    assert!(!layer.join("loser").exists());
    assert!(staging_entries().is_empty());
}

/// Test that a whiteout that points out of the layer does not delete host
/// files, so that a hostile image cannot harm the host.
///   1. Write files in a host directory out of the layer cache
///   2. Cache layers with whiteouts through a symlink, an absolute path and
///      `..` path parts
///   3. Check that all host files are unchanged
#[test]
fn whiteout_aimed_outside_layer_does_not_touch_host_files() {
    let home = TempHome::new();
    let outside = home.path().join("outside");
    std::fs::create_dir_all(&outside).unwrap();
    for victim in ["via-symlink", "via-absolute", "via-parent"] {
        std::fs::write(outside.join(victim), b"precious").unwrap();
    }

    let through_symlink = LayerTar::default()
        .symlink("esc", &outside)
        .file("esc/.wh.via-symlink", "")
        .gz();
    // The extraction can fail or succeed. Only the host files matter.
    let _ = layer::ensure_layer_cached(
        "sha256:evil-symlink",
        |dest| Ok(std::fs::write(dest, &through_symlink)?),
        None,
    );
    let absolute = LayerTar::default()
        .raw_file(&format!("{}/.wh.via-absolute", outside.display()))
        .gz();
    let _ = layer::ensure_layer_cached(
        "sha256:evil-absolute",
        |dest| Ok(std::fs::write(dest, &absolute)?),
        None,
    );
    let parent = LayerTar::default()
        .raw_file("../../../../../outside/.wh.via-parent")
        .gz();
    let _ = layer::ensure_layer_cached(
        "sha256:evil-parent",
        |dest| Ok(std::fs::write(dest, &parent)?),
        None,
    );

    for victim in ["via-symlink", "via-absolute", "via-parent"] {
        assert_eq!(
            std::fs::read(outside.join(victim)).unwrap(),
            b"precious",
            "{victim}"
        );
    }
}

/// Test that a whiteout that names its own directory or the parent
/// directory refuses the layer, so that a hostile image cannot empty the
/// shared layer cache.
///   1. Cache a good layer
///   2. Cache layers with the whiteouts `.wh.`, `.wh..` and `.wh...`
///   3. Check that each of these layers is refused
///   4. Check that the good layer is unchanged
#[test]
fn whiteout_naming_own_or_parent_directory_is_refused() {
    let _home = TempHome::new();
    let good = cache_layer("sha256:good", &LayerTar::default().file("keep", "yes").gz());

    for (digest, name) in [
        ("sha256:evil-empty", "dir/.wh."),
        ("sha256:evil-dot", "dir/.wh.."),
        ("sha256:evil-dotdot", "dir/.wh..."),
    ] {
        let tar = LayerTar::default().raw_file(name).gz();
        let res = layer::ensure_layer_cached(digest, |dest| Ok(std::fs::write(dest, &tar)?), None);
        let err = res.expect_err(name).to_string();
        assert!(err.contains("unsafe whiteout name"), "{name}: {err}");
    }

    let keep = cache::layer_dir(&good).unwrap().join("keep");
    assert_eq!(std::fs::read(keep).unwrap(), b"yes");
}
