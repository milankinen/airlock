//! Tests for the cached image entries, the sandbox image links and the
//! garbage collector of the OCI cache.

use airlock_test_utils::temp_dir;

use super::*;

/// Cache a small layer as `digest` and return its layer key.
fn small_layer(data_dir: &DataDir, digest: &str) -> String {
    cache_layer(
        data_dir,
        digest,
        &LayerTar::default().file("marker", "x").gz(),
    )
}

/// Whether the layer `key` exists in the layer cache.
fn layer_exists(data_dir: &DataDir, key: &str) -> bool {
    real(&cache::layer_dir(data_dir, key).unwrap()).exists()
}

/// Test that a cached image is ready only while all its layers exist, so
/// that a sweep that removed a layer makes airlock resolve the image again.
///   1. Write a cached image with one layer and check that it is ready
///   2. Remove the layer and check that the entry reads but is not ready
///   3. Cache the layer again and check that the image is ready
///   4. Check that an image with no layers is never ready
#[test]
fn cached_image_is_ready_only_while_all_its_layers_exist() {
    let tmp = temp_dir();
    let data_dir = &data_dir_at(tmp.path());
    let key = small_layer(data_dir, "sha256:L1");
    let path = cache::image_path(data_dir, "sha256:swept").unwrap();
    write_cached_image(
        data_dir,
        "sha256:swept",
        &image("sha256:swept", vec![key.clone()]),
    )
    .unwrap();
    assert!(read_ready_image(data_dir, &path).is_some());

    std::fs::remove_dir_all(real(&cache::layer_dir(data_dir, &key).unwrap())).unwrap();
    assert!(read_cached_image(&path).is_some());
    assert!(read_ready_image(data_dir, &path).is_none());

    small_layer(data_dir, "sha256:L1");
    assert!(read_ready_image(data_dir, &path).is_some());

    let empty = cache::image_path(data_dir, "sha256:empty").unwrap();
    write_cached_image(data_dir, "sha256:empty", &image("sha256:empty", vec![])).unwrap();
    assert!(read_ready_image(data_dir, &empty).is_none());
}

/// Test that the GC sweep keeps only the images that a sandbox links to and
/// their layers, and removes all staging leftovers.
///   1. Cache a live image (linked by a sandbox) and an orphan image that
///      share one layer
///   2. Add staging leftovers to the layer cache
///   3. Sweep and check that only the live image and its layers stay
///   4. Remove the sandbox, sweep again and check that all is gone
#[test]
fn gc_sweep_keeps_only_images_linked_by_sandboxes_and_their_layers() {
    let home = temp_dir();
    let data_dir = &data_dir_at(home.path());
    let shared = small_layer(data_dir, "sha256:shared");
    let own = small_layer(data_dir, "sha256:own");
    let orphaned = small_layer(data_dir, "sha256:orphaned");
    let live = image("sha256:live", vec![own.clone(), shared.clone()]);
    let orphan = image("sha256:orphan", vec![orphaned.clone(), shared.clone()]);
    let live_path = real(&cache::image_path(data_dir, &live.image_id).unwrap());
    let orphan_path = real(&cache::image_path(data_dir, &orphan.image_id).unwrap());
    write_cached_image(data_dir, &orphan.image_id, &orphan).unwrap();
    let sandbox = data_dir.sandboxes.join("box").unwrap();
    std::fs::create_dir_all(real(&sandbox)).unwrap();
    let sandbox_image = sandbox.join("image").unwrap();
    // The second call must see the existing link and do nothing.
    ensure_image_hardlink(data_dir, &sandbox_image, &live).unwrap();
    ensure_image_hardlink(data_dir, &sandbox_image, &live).unwrap();
    let layers = real(&cache::layers_root(data_dir).unwrap());
    std::fs::write(layers.join("abc.download.tmp"), b"").unwrap();
    std::fs::write(layers.join("def.download"), b"").unwrap();
    std::fs::create_dir_all(layers.join("ghi.tmp")).unwrap();

    gc_sweep(data_dir);

    assert!(live_path.exists());
    assert!(!orphan_path.exists());
    assert!(layer_exists(data_dir, &own) && layer_exists(data_dir, &shared));
    assert!(!layer_exists(data_dir, &orphaned));
    let mut left: Vec<_> = std::fs::read_dir(&layers)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    left.sort();
    let mut expected = vec![own.clone(), shared.clone()];
    expected.sort();
    assert_eq!(left, expected);

    std::fs::remove_dir_all(real(&sandbox)).unwrap();
    gc_sweep(data_dir);

    assert!(!live_path.exists());
    assert!(!layer_exists(data_dir, &own) && !layer_exists(data_dir, &shared));
}

/// Test that the sandbox image link is made again when the cache entry is
/// gone, so that the GC does not remove the image of a live sandbox.
///   1. Link a sandbox to a cached image
///   2. Remove the cache entry and link again
///   3. Sweep and check that the image entry exists and is ready
#[test]
fn sandbox_image_link_is_restored_when_cache_entry_was_wiped() {
    let home = temp_dir();
    let data_dir = &data_dir_at(home.path());
    let live = image("sha256:live", vec![small_layer(data_dir, "sha256:L1")]);
    let live_path = cache::image_path(data_dir, &live.image_id).unwrap();
    let sandbox_image = data_dir.root.join("image").unwrap();
    ensure_image_hardlink(data_dir, &sandbox_image, &live).unwrap();

    std::fs::remove_file(real(&live_path)).unwrap();
    ensure_image_hardlink(data_dir, &sandbox_image, &live).unwrap();

    gc_sweep(data_dir);
    assert_eq!(
        read_ready_image(data_dir, &live_path).unwrap().image_id,
        "sha256:live"
    );
}

/// Test that a cache entry from before the `user` field loads with no user,
/// so that airlock knows to resolve the user again.
///   1. Write a cache entry without the `user` field
///   2. Check that it loads with no user
///   3. Write a current entry and check that it has a user
#[test]
fn cache_file_from_before_user_field_loads_as_legacy() {
    let tmp = temp_dir();
    let data_dir = &data_dir_at(tmp.path());
    let path = cache::image_path(data_dir, "sha256:legacy").unwrap();
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
    std::fs::write(real(&path), serde_json::to_vec(&legacy).unwrap()).unwrap();
    assert_eq!(read_cached_image(&path).unwrap().user, None);

    write_cached_image(
        data_dir,
        "sha256:legacy",
        &image("sha256:legacy", vec![cache::layer_key("sha256:L1")]),
    )
    .unwrap();
    assert_eq!(read_cached_image(&path).unwrap().user, Some(String::new()));
}

/// Test that a symlink at the `image` file of a sandbox is not followed. A
/// followed link lets airlock read or link a host file.
///   1. Write a host file and put a symlink to it at the sandbox `image`
///   2. Check that the image read ignores the link
///   3. Link the sandbox to a cached image
///   4. Check that the host file did not change and that `image` is now a
///      hard link to the cache entry
#[test]
fn planted_sandbox_image_symlink_is_not_followed() {
    use std::os::unix::fs::MetadataExt;

    let home = temp_dir();
    let data_dir = &data_dir_at(home.path());
    let victim = home.path().join("victim.json");
    let live = image("sha256:live", vec![small_layer(data_dir, "sha256:L1")]);
    let json = serde_json::to_vec(&CachedImage::V2(live.clone())).unwrap();
    std::fs::write(&victim, &json).unwrap();
    std::os::unix::fs::symlink(&victim, home.path().join("image")).unwrap();
    let sandbox_image = data_dir.root.join("image").unwrap();

    assert!(read_cached_image(&sandbox_image).is_none());
    ensure_image_hardlink(data_dir, &sandbox_image, &live).unwrap();

    assert_eq!(std::fs::read(&victim).unwrap(), json);
    let link = std::fs::symlink_metadata(home.path().join("image")).unwrap();
    let entry =
        std::fs::metadata(real(&cache::image_path(data_dir, &live.image_id).unwrap())).unwrap();
    assert!(link.is_file());
    assert_eq!((link.dev(), link.ino()), (entry.dev(), entry.ino()));
}

/// Test that the sweep does not follow a symlink or open a FIFO in the
/// images dir. A followed link reads a file that can keep layers live, and
/// an open of a FIFO blocks the sweep.
///   1. Write a host file that lists a layer, and link to it from the
///      images dir
///   2. Make a FIFO in the images dir
///   3. Sweep and check that the layer is gone and both entries stay
#[test]
fn gc_sweep_skips_symlink_and_fifo_in_images_dir() {
    let home = temp_dir();
    let data_dir = &data_dir_at(home.path());
    let layer = small_layer(data_dir, "sha256:L1");
    let host = home.path().join("host.json");
    let json = serde_json::to_vec(&CachedImage::V2(image("sha256:x", vec![layer.clone()])));
    std::fs::write(&host, json.unwrap()).unwrap();
    let images = real(&cache::images_root(data_dir).unwrap());
    std::os::unix::fs::symlink(&host, images.join("link")).unwrap();
    let fifo =
        std::ffi::CString::new(images.join("fifo").into_os_string().into_encoded_bytes()).unwrap();
    // SAFETY: `fifo` is a valid C string.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    gc_sweep(data_dir);

    assert!(!layer_exists(data_dir, &layer));
    assert!(std::fs::symlink_metadata(images.join("link")).is_ok());
    assert!(std::fs::symlink_metadata(images.join("fifo")).is_ok());
    assert!(host.exists());
}

/// Test that `--yes` never re-creates an imported sandbox with a disk and an
/// unknown old image, because the re-create deletes the imported disk.
///   1. Make a sandbox dir with a disk and an empty import marker
///   2. Check that `Recreate` stops with `ImportedImageUnknown`, and that
///      `Ask` and `Refuse` stay
///   3. Write a known digest into the marker and check that `Recreate` stays
///   4. Remove the disk and check that `Recreate` stays
#[test]
fn recreate_of_imported_sandbox_with_unknown_image_stops() {
    let tmp = temp_dir();
    let dir = crate::test_cfg::oci::verified_dir(tmp.path());
    std::fs::write(tmp.path().join(crate::vm::disk::DISK_FILE), "disk").unwrap();
    std::fs::write(tmp.path().join(IMPORT_MARKER), "").unwrap();

    assert!(matches!(
        guard_recreate(&dir, OnImageChange::Recreate),
        Err(ImageChangeStop::ImportedImageUnknown)
    ));
    for mode in [OnImageChange::Ask, OnImageChange::Refuse] {
        assert_eq!(guard_recreate(&dir, mode).unwrap(), mode);
    }

    let digest = format!("sha256:{}", "a".repeat(64));
    std::fs::write(tmp.path().join(IMPORT_MARKER), digest).unwrap();
    assert_eq!(
        guard_recreate(&dir, OnImageChange::Recreate).unwrap(),
        OnImageChange::Recreate
    );

    std::fs::write(tmp.path().join(IMPORT_MARKER), "").unwrap();
    std::fs::remove_file(tmp.path().join(crate::vm::disk::DISK_FILE)).unwrap();
    assert_eq!(
        guard_recreate(&dir, OnImageChange::Recreate).unwrap(),
        OnImageChange::Recreate
    );
}
