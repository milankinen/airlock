use super::*;
use crate::test_cfg::home::TempHome;

fn small_layer(digest: &str) -> String {
    cache_layer(digest, &LayerTar::default().file("marker", "x").gz())
}

fn layer_exists(key: &str) -> bool {
    cache::layer_dir(key).unwrap().exists()
}

#[test]
fn cached_image_is_ready_only_while_all_its_layers_exist() {
    let _home = TempHome::new();
    let key = small_layer("sha256:L1");
    let path = cache::image_path("sha256:swept").unwrap();
    write_cached_image(&path, &image("sha256:swept", vec![key.clone()])).unwrap();
    assert!(read_ready_image(&path).is_some());

    std::fs::remove_dir_all(cache::layer_dir(&key).unwrap()).unwrap();
    assert!(read_cached_image(&path).is_some());
    assert!(read_ready_image(&path).is_none());

    small_layer("sha256:L1");
    assert!(read_ready_image(&path).is_some());

    let empty = cache::image_path("sha256:empty").unwrap();
    write_cached_image(&empty, &image("sha256:empty", vec![])).unwrap();
    assert!(read_ready_image(&empty).is_none());
}

#[test]
fn gc_sweep_keeps_only_images_linked_by_sandboxes_and_their_layers() {
    let home = TempHome::new();
    let shared = small_layer("sha256:shared");
    let own = small_layer("sha256:own");
    let orphaned = small_layer("sha256:orphaned");
    let live = image("sha256:live", vec![own.clone(), shared.clone()]);
    let orphan = image("sha256:orphan", vec![orphaned.clone(), shared.clone()]);
    let live_path = cache::image_path(&live.image_id).unwrap();
    let orphan_path = cache::image_path(&orphan.image_id).unwrap();
    write_cached_image(&orphan_path, &orphan).unwrap();
    let sandbox = home.path().join("project/.airlock/sandbox");
    std::fs::create_dir_all(&sandbox).unwrap();
    ensure_image_hardlink(&sandbox.join("image"), &live_path, &live).unwrap();
    ensure_image_hardlink(&sandbox.join("image"), &live_path, &live).unwrap();
    let layers = cache::layers_root().unwrap();
    std::fs::write(layers.join("abc.download.tmp"), b"").unwrap();
    std::fs::write(layers.join("def.download"), b"").unwrap();
    std::fs::create_dir_all(layers.join("ghi.tmp")).unwrap();

    gc_sweep();

    assert!(live_path.exists());
    assert!(!orphan_path.exists());
    assert!(layer_exists(&own) && layer_exists(&shared));
    assert!(!layer_exists(&orphaned));
    let mut left: Vec<_> = std::fs::read_dir(&layers)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    let mut expected = vec![own.clone(), shared.clone()];
    expected.sort();
    assert_eq!(left, expected);

    std::fs::remove_dir_all(home.path().join("project/.airlock")).unwrap();
    gc_sweep();

    assert!(!live_path.exists());
    assert!(!layer_exists(&own) && !layer_exists(&shared));
}

#[test]
fn sandbox_image_link_is_restored_when_cache_entry_was_wiped() {
    let home = TempHome::new();
    let live = image("sha256:live", vec![small_layer("sha256:L1")]);
    let live_path = cache::image_path(&live.image_id).unwrap();
    let sandbox_image = home.path().join("image");
    ensure_image_hardlink(&sandbox_image, &live_path, &live).unwrap();

    std::fs::remove_file(&live_path).unwrap();
    ensure_image_hardlink(&sandbox_image, &live_path, &live).unwrap();

    gc_sweep();
    assert_eq!(
        read_ready_image(&live_path).unwrap().image_id,
        "sha256:live"
    );
}

#[test]
fn cache_file_from_before_user_field_loads_as_legacy() {
    let _home = TempHome::new();
    let path = cache::image_path("sha256:legacy").unwrap();
    let legacy = serde_json::json!({
        "schema": "v2",
        "image_id": "sha256:legacy",
        "name": "node:22",
        "image_layers": [cache::layer_key("sha256:L1")],
        "container_home": "/root",
        "uid": 0,
        "gid": 0,
        "cmd": ["/bin/sh"],
        "env": [],
    });
    std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(read_cached_image(&path).unwrap().user, None);

    write_cached_image(
        &path,
        &image("sha256:legacy", vec![cache::layer_key("sha256:L1")]),
    )
    .unwrap();
    assert_eq!(read_cached_image(&path).unwrap().user, Some(String::new()));
}
