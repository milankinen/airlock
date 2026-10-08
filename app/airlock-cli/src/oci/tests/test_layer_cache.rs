use super::*;
use crate::test_cfg::home::TempHome;

fn xattr_of(path: &Path, name: &str) -> Option<Vec<u8>> {
    xattr::get(path, name).unwrap()
}

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
