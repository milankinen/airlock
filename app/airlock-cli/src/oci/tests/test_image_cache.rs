//! Tests for the cached image entries, the image records and the garbage
//! collector of the OCI cache.

use airlock_test_utils::temp_dir;

use super::*;
use crate::oci::record;
use crate::test_cfg::vault::{MemoryStorage, vault_with};

/// A valid sandbox id for the tests.
const BOX_ID: &str = "abcd2345";

/// Open the database of `data_dir`.
fn open_db(data_dir: &DataDir) -> Db {
    Db::open(&data_dir.db).unwrap()
}

/// Make the directory of the sandbox `id` in `data_dir`.
fn sandbox_dir(data_dir: &DataDir, id: &str) -> VerifiedPath {
    let dir = data_dir.sandboxes.join(id).unwrap();
    std::fs::create_dir_all(real(&dir)).unwrap();
    dir
}

/// The image record of an image `digest` with the configured `name`.
fn named(name: &str, digest: &str) -> ImageRecord {
    ImageRecord {
        digest: digest.to_string(),
        name: Some(name.to_string()),
    }
}

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

/// Test that the GC sweep keeps only the images that an image record names
/// and their layers, removes all staging leftovers, and removes the record
/// of a sandbox that is gone.
///   1. Cache a live image (named by a sandbox record) and an orphan image
///      that share one layer
///   2. Add staging leftovers to the layer cache
///   3. Sweep and check that only the live image and its layers stay
///   4. Remove the sandbox directory, sweep again and check that all is gone,
///      also the record
#[tokio::test]
async fn gc_sweep_keeps_only_images_named_by_records_and_their_layers() {
    let home = temp_dir();
    let data_dir = &data_dir_at(home.path());
    let db = open_db(data_dir);
    let shared = small_layer(data_dir, "sha256:shared");
    let own = small_layer(data_dir, "sha256:own");
    let orphaned = small_layer(data_dir, "sha256:orphaned");
    let live = image("sha256:live", vec![own.clone(), shared.clone()]);
    let orphan = image("sha256:orphan", vec![orphaned.clone(), shared.clone()]);
    let live_path = real(&cache::image_path(data_dir, &live.image_id).unwrap());
    let orphan_path = real(&cache::image_path(data_dir, &orphan.image_id).unwrap());
    write_cached_image(data_dir, &live.image_id, &live).unwrap();
    write_cached_image(data_dir, &orphan.image_id, &orphan).unwrap();
    let sandbox = sandbox_dir(data_dir, BOX_ID);
    let owner = ImageOwner::Sandbox(BOX_ID.to_string());
    record::set(&db, &owner, named("alpine:3.19", &live.image_id))
        .await
        .unwrap();
    let layers = real(&cache::layers_root(data_dir).unwrap());
    std::fs::write(layers.join("abc.download.tmp"), b"").unwrap();
    std::fs::write(layers.join("def.download"), b"").unwrap();
    std::fs::create_dir_all(layers.join("ghi.tmp")).unwrap();

    gc_sweep(&db, data_dir).await;

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
    gc_sweep(&db, data_dir).await;

    assert!(!live_path.exists());
    assert!(!layer_exists(data_dir, &own) && !layer_exists(data_dir, &shared));
    assert_eq!(record::get(&db, &owner).await.unwrap(), None);
}

/// Test that the image files of an older version become image records, so
/// that a sweep keeps the images of sandboxes and templates that did not
/// start since the update.
///   1. Cache two images. Give a sandbox and a template the image file of an
///      older version for them, and give a second sandbox an import marker
///   2. Sweep and check that both images stay
///   3. Check the records, and that the old files are gone
///   4. Start the first sandbox with no registry and check that it uses the
///      cached image
#[tokio::test]
async fn image_files_of_older_version_become_records_and_keep_images() {
    let home = temp_dir();
    let data_dir = &data_dir_at(home.path());
    let db = open_db(data_dir);
    let layer = small_layer(data_dir, "sha256:L1");
    let mut started = image("sha256:started", vec![layer.clone()]);
    let template_image = image("sha256:template", vec![layer]);
    for img in [&started, &template_image] {
        write_cached_image(data_dir, &img.image_id, img).unwrap();
    }
    // An older version wrote the name into the image file.
    let legacy = |img: &OciImage, name: &str| {
        let mut json = serde_json::to_value(CachedImage::V3(img.clone())).unwrap();
        json["name"] = name.into();
        json.to_string()
    };
    let sandbox = sandbox_dir(data_dir, BOX_ID);
    std::fs::write(
        real(&sandbox).join("image"),
        legacy(&started, "127.0.0.1:1/alpine:3"),
    )
    .unwrap();
    let template = data_dir.templates.join("base").unwrap();
    std::fs::create_dir_all(real(&template)).unwrap();
    std::fs::write(
        real(&template).join("image"),
        legacy(&template_image, "alpine:edge"),
    )
    .unwrap();
    let imported = sandbox_dir(data_dir, "imported");
    std::fs::write(real(&imported).join(IMPORT_MARKER), "sha256:old\n").unwrap();

    gc_sweep(&db, data_dir).await;

    for img in [&started, &template_image] {
        assert!(real(&cache::image_path(data_dir, &img.image_id).unwrap()).exists());
    }
    let owner = ImageOwner::Sandbox(BOX_ID.to_string());
    assert_eq!(
        record::get(&db, &owner).await.unwrap(),
        Some(named("127.0.0.1:1/alpine:3", "sha256:started"))
    );
    let base = ImageOwner::Template("base".into());
    assert_eq!(
        record::get(&db, &base).await.unwrap(),
        Some(named("alpine:edge", "sha256:template"))
    );
    let imported_owner = ImageOwner::Sandbox("imported".into());
    assert_eq!(
        record::get(&db, &imported_owner).await.unwrap(),
        Some(ImageRecord {
            digest: "sha256:old".into(),
            name: None
        })
    );
    for file in ["image", IMPORT_MARKER] {
        for dir in [&sandbox, &template, &imported] {
            assert!(!real(dir).join(file).exists(), "{dir}/{file}");
        }
    }

    let vault = vault_with(MemoryStorage::default(), &[]);
    let cfg = ImageRef {
        resolution: Resolution::Registry,
        insecure: true,
        ..ImageRef::auto("127.0.0.1:1/alpine:3")
    };
    let prepared = prepare(
        &db,
        data_dir,
        &owner,
        &sandbox,
        &cfg,
        &vault,
        OnImageChange::Refuse,
    )
    .await
    .unwrap();
    started.name.clone_from(&cfg.name);
    assert_eq!(prepared.change, ImageChange::Unchanged);
    assert_eq!(prepared.image.image_id, started.image_id);
    assert_eq!(prepared.image.name, started.name);
}

/// Test that a symlink at the image file of an older version is not
/// followed. A followed link lets airlock read a host file as the record.
///   1. Write a host file with image JSON, and put a symlink to it at the
///      sandbox `image`
///   2. Read the record and check that there is none and the host file did
///      not change
#[tokio::test]
async fn planted_image_file_symlink_is_not_followed() {
    let home = temp_dir();
    let data_dir = &data_dir_at(home.path());
    let db = open_db(data_dir);
    let victim = home.path().join("victim.json");
    let json = serde_json::to_vec(&CachedImage::V3(image("sha256:x", vec![]))).unwrap();
    std::fs::write(&victim, &json).unwrap();
    let sandbox = sandbox_dir(data_dir, BOX_ID);
    std::os::unix::fs::symlink(&victim, real(&sandbox).join("image")).unwrap();

    let owner = ImageOwner::Sandbox(BOX_ID.to_string());
    assert_eq!(record::load(&db, &owner, &sandbox).await.unwrap(), None);
    assert_eq!(std::fs::read(&victim).unwrap(), json);
}

/// Test that the sweep does not follow a symlink or open a FIFO in the
/// images dir. A followed link reads a file that can keep layers live, and
/// an open of a FIFO blocks the sweep.
///   1. Write a host file that lists a layer, and link to it from the
///      images dir
///   2. Make a FIFO in the images dir
///   3. Sweep and check that the layer is gone and both entries stay
#[tokio::test]
async fn gc_sweep_skips_symlink_and_fifo_in_images_dir() {
    let home = temp_dir();
    let data_dir = &data_dir_at(home.path());
    let layer = small_layer(data_dir, "sha256:L1");
    let host = home.path().join("host.json");
    let json = serde_json::to_vec(&CachedImage::V3(image("sha256:x", vec![layer.clone()])));
    std::fs::write(&host, json.unwrap()).unwrap();
    let images = real(&cache::images_root(data_dir).unwrap());
    std::os::unix::fs::symlink(&host, images.join("link")).unwrap();
    let fifo =
        std::ffi::CString::new(images.join("fifo").into_os_string().into_encoded_bytes()).unwrap();
    // SAFETY: `fifo` is a valid C string.
    assert_eq!(unsafe { libc::mkfifo(fifo.as_ptr(), 0o600) }, 0);

    gc_sweep(&open_db(data_dir), data_dir).await;

    assert!(!layer_exists(data_dir, &layer));
    assert!(std::fs::symlink_metadata(images.join("link")).is_ok());
    assert!(std::fs::symlink_metadata(images.join("fifo")).is_ok());
    assert!(host.exists());
}

/// Test that `--yes` never re-creates an imported sandbox with a disk and an
/// unknown old image, because the re-create deletes the imported disk.
///   1. Make a sandbox dir with a disk and a record with no digest
///   2. Check that `Recreate` stops with `ImportedImageUnknown`, and that
///      `Ask` and `Refuse` stay
///   3. Check that a known digest keeps `Recreate`
///   4. Remove the disk and check that `Recreate` stays
#[test]
fn recreate_of_imported_sandbox_with_unknown_image_stops() {
    let tmp = temp_dir();
    let dir = crate::test_cfg::oci::verified_dir(tmp.path());
    std::fs::write(tmp.path().join(crate::vm::disk::DISK_FILE), "disk").unwrap();
    let unknown = ImageRecord {
        digest: String::new(),
        name: None,
    };

    assert!(matches!(
        guard_recreate(&unknown, &dir, OnImageChange::Recreate),
        Err(ImageChangeStop::ImportedImageUnknown)
    ));
    for mode in [OnImageChange::Ask, OnImageChange::Refuse] {
        assert_eq!(guard_recreate(&unknown, &dir, mode).unwrap(), mode);
    }

    let known = ImageRecord {
        digest: format!("sha256:{}", "a".repeat(64)),
        name: None,
    };
    assert_eq!(
        guard_recreate(&known, &dir, OnImageChange::Recreate).unwrap(),
        OnImageChange::Recreate
    );

    std::fs::remove_file(tmp.path().join(crate::vm::disk::DISK_FILE)).unwrap();
    assert_eq!(
        guard_recreate(&unknown, &dir, OnImageChange::Recreate).unwrap(),
        OnImageChange::Recreate
    );
}

/// Test that only an `if-not-present` tag with a registry source pins the
/// stored digest.
///   1. Check that an `if-not-present` tag gets the pin
///   2. Check that `if-changed`, a pinned name, local-only resolution and a
///      missing or bad digest give no pin
#[test]
fn stored_digest_is_pinned_only_for_if_not_present_tag_with_registry_source() {
    let digest = format!("sha256:{}", "a".repeat(64));
    let tag = ImageRef::auto("alpine:3");
    assert_eq!(
        stored_pin(&tag, Some(&named("alpine:3", &digest))).map(|r| r.name),
        Some(format!("alpine:3@{digest}"))
    );

    let if_changed = ImageRef {
        pull_policy: PullPolicy::IfChanged,
        ..tag.clone()
    };
    let pinned = ImageRef::auto(format!("alpine:3@sha256:{}", "b".repeat(64)));
    let docker = ImageRef {
        resolution: Resolution::Docker,
        ..tag.clone()
    };
    for cfg in [&if_changed, &pinned, &docker] {
        let stored = named(&cfg.name, &digest);
        assert!(stored_pin(cfg, Some(&stored)).is_none(), "{cfg:?}");
    }
    assert!(stored_pin(&tag, None).is_none());
    assert!(stored_pin(&tag, Some(&named("alpine:3", "sha256:short"))).is_none());
}

/// Test that a changed tag does not pin the stored digest. The registry
/// resolves a pin by its digest only, and the old digest is in the same
/// repository, so a pin gives the old image.
///   1. Check that an image stored with another tag gives no pin
///   2. Check that an imported sandbox, which stores no name, gets the pin
#[test]
fn stored_digest_of_another_tag_is_not_pinned() {
    let digest = format!("sha256:{}", "a".repeat(64));
    let tag = ImageRef::auto("debian:trixie-slim");
    let other = named("debian:bookworm-slim", &digest);
    assert!(stored_pin(&tag, Some(&other)).is_none());

    let imported = ImageRecord {
        digest: digest.clone(),
        name: None,
    };
    assert_eq!(
        stored_pin(&tag, Some(&imported)).map(|r| r.name),
        Some(format!("debian:trixie-slim@{digest}"))
    );
}

/// Test that two sandboxes with different names for one digest each keep
/// their own name. The cached image is shared, but each sandbox has its own
/// record. Otherwise a moved tag gives an image change although the config
/// did not change, and `airlock exec` finds no image.
///   1. Cache one image, and give two sandboxes records with different names
///      for it
///   2. Start each sandbox two times. No registry is available, so a start
///      works only with the cached image
///   3. Check that each sandbox has the image for its own name only
#[tokio::test]
async fn sandboxes_with_different_names_for_one_image_keep_own_names() {
    let home = temp_dir();
    let data_dir = &data_dir_at(home.path());
    let db = open_db(data_dir);
    let vault = vault_with(MemoryStorage::default(), &[]);
    let bookworm = ImageRef {
        resolution: Resolution::Registry,
        insecure: true,
        ..ImageRef::auto("127.0.0.1:1/debian:bookworm")
    };
    let twelve = ImageRef {
        name: "127.0.0.1:1/debian:12".to_string(),
        ..bookworm.clone()
    };
    let shared = image("sha256:shared", vec![small_layer(data_dir, "sha256:L1")]);
    write_cached_image(data_dir, &shared.image_id, &shared).unwrap();
    let boxes = [(BOX_ID, &bookworm), ("efgh2345", &twelve)].map(|(id, cfg)| {
        (
            ImageOwner::Sandbox(id.to_string()),
            sandbox_dir(data_dir, id),
            cfg,
        )
    });
    for (owner, _, cfg) in &boxes {
        record::set(&db, owner, named(&cfg.name, &shared.image_id))
            .await
            .unwrap();
    }

    for _ in 0..2 {
        for (owner, dir, cfg) in &boxes {
            let prepared = prepare(
                &db,
                data_dir,
                owner,
                dir,
                cfg,
                &vault,
                OnImageChange::Refuse,
            )
            .await
            .unwrap();
            assert_eq!(prepared.change, ImageChange::Unchanged);
            assert_eq!(prepared.image.name, cfg.name);
        }
    }
    for (owner, dir, cfg) in &boxes {
        let load = |cfg| load_sandbox_image(&db, data_dir, owner, dir, cfg);
        assert!(load(cfg).await.unwrap().is_some());
        let other = if cfg.name == bookworm.name {
            &twelve
        } else {
            &bookworm
        };
        assert!(load(other).await.unwrap().is_none());
    }
}
