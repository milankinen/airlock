//! Tests for the import of local images from a `docker save` or podman
//! export: layer staging, cached layers and blob digest checks.

use airlock_test_utils::temp_dir;
use sha2::{Digest, Sha256};

use super::*;

/// The SHA-256 of `bytes` as lowercase hex.
fn sha256_hex(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

/// An image config with the user `user` and the uncompressed layers
/// `layers` (hex digests, base first).
fn config_json(user: &str, layers: &[&str]) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "architecture": "amd64",
        "os": "linux",
        "config": {
            "User": user,
            "Entrypoint": ["docker-entrypoint.sh"],
            "Cmd": ["node"],
            "Env": ["NODE_VERSION=22.1.0"],
        },
        "rootfs": {
            "type": "layers",
            "diff_ids": layers.iter().map(|h| format!("sha256:{h}")).collect::<Vec<_>>(),
        },
    }))
    .unwrap()
}

/// Extract the staged layer tarballs of `save` into the layer cache, as
/// `ensure_local_image` does.
/// Returns:
///   The layer keys, topmost first.
fn extract_saved_layers(data_dir: &Path, save: &docker::DockerSave) -> Vec<String> {
    let mut keys: Vec<String> = save
        .layer_digests
        .iter()
        .map(|digest| {
            layer::ensure_layer_cached(
                data_dir,
                digest,
                |_| panic!("the docker export already staged {digest}"),
                None,
            )
            .unwrap();
            cache::layer_key(digest)
        })
        .collect();
    keys.reverse();
    keys
}

/// The path of the staged tarball of the layer `hex`.
fn staged(data_dir: &Path, hex: &str) -> std::path::PathBuf {
    cache::layers_root(data_dir).unwrap().join(format!(
        "{}.download",
        cache::layer_key(&format!("sha256:{hex}"))
    ))
}

/// The names of temporary staging files left in the layer cache.
fn staging_leftovers(data_dir: &Path) -> Vec<String> {
    std::fs::read_dir(cache::layers_root(data_dir).unwrap())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| {
            std::path::Path::new(name)
                .extension()
                .is_some_and(|ext| ext == "tmp")
        })
        .collect()
}

/// Test that a podman `docker-archive` export becomes an image with the
/// user, home, command and env of its config, and that its staging files
/// go away.
///   1. Make an export with a base layer that declares the `node` user, a
///      top layer and a config with `User: node`
///   2. Stage the export and check the layer digests and staged files
///   3. Extract the layers and check that no staging file is left
///   4. Build the image and check the user, home, command, env, OS and
///      layer contents
#[test]
fn podman_archive_export_becomes_image_with_named_user() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let base = LayerTar::default()
        .file(
            "etc/passwd",
            "root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1000:Node:/home/node:/bin/sh\n",
        )
        .file("etc/group", "root:x:0:\nnode:x:1000:\n")
        .file("etc/os-release", "ID=debian\n")
        .plain();
    let top = LayerTar::default()
        .file("usr/local/bin/node", "#!/bin/sh\n")
        .plain();
    let (base_hex, top_hex) = (sha256_hex(&base), sha256_hex(&top));
    let config = config_json("node", &[&base_hex, &top_hex]);
    let config_hex = sha256_hex(&config);
    let manifest = format!(
        r#"[{{"Config":"{config_hex}.json","RepoTags":["localhost/x:1"],"Layers":["{base_hex}.tar","{top_hex}.tar"]}}]"#
    );
    let export = LayerTar::default()
        .file(&format!("{base_hex}.tar"), &base)
        .file(&format!("{top_hex}.tar"), &top)
        .file(&format!("{config_hex}.json"), &config)
        .file("manifest.json", manifest)
        .file("repositories", "{}")
        .plain();

    let save = docker::save_from_stream(data_dir, std::io::Cursor::new(export)).unwrap();
    assert_eq!(
        save.layer_digests,
        [format!("sha256:{base_hex}"), format!("sha256:{top_hex}")]
    );
    assert!(staged(data_dir, &base_hex).is_file());

    let layers = extract_saved_layers(data_dir, &save);
    assert!(!staged(data_dir, &base_hex).exists());
    assert!(staging_leftovers(data_dir).is_empty());
    let image = build_oci_image(
        data_dir,
        "sha256:img".into(),
        "localhost/x:1".into(),
        layers.clone(),
        &save.image_config,
    )
    .unwrap();
    assert_eq!((image.uid, image.gid), (1000, 1000));
    assert_eq!(image.container_home, "/home/node");
    assert_eq!(image.cmd, ["docker-entrypoint.sh", "node"]);
    assert!(image.env.contains(&"HOME=/home/node".to_string()));
    assert!(image.env.contains(&"NODE_VERSION=22.1.0".to_string()));
    assert_eq!(os_release(data_dir, &image).unwrap().id, "debian");
    assert!(
        cache::layer_dir(data_dir, &layers[0])
            .unwrap()
            .join("usr/local/bin/node")
            .is_file()
    );
}

/// Test that a Docker OCI layout export stages only layers that are not in
/// the cache, and ignores the legacy `<id>/layer.tar` members.
///   1. Put one layer in the cache
///   2. Make an export with the cached layer, a new layer and a legacy
///      member
///   3. Stage the export and check that only the new layer is staged
///   4. Extract the layers and check both layer contents and that no
///      staging file is left
#[test]
fn docker_oci_layout_export_skips_cached_layers_and_legacy_members() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let cached = LayerTar::default().file("cached", "1").plain();
    let fresh = LayerTar::default().file("fresh", "2").plain();
    let (cached_hex, fresh_hex) = (sha256_hex(&cached), sha256_hex(&fresh));
    cache_layer(data_dir, &format!("sha256:{cached_hex}"), &cached);
    let config = config_json("", &[&cached_hex, &fresh_hex]);
    let config_hex = sha256_hex(&config);
    let manifest = format!(
        r#"[{{"Config":"blobs/sha256/{config_hex}","Layers":["blobs/sha256/{cached_hex}","blobs/sha256/{fresh_hex}"]}}]"#
    );
    let export = LayerTar::default()
        .file(&format!("blobs/sha256/{cached_hex}"), &cached)
        .file(&format!("blobs/sha256/{fresh_hex}"), &fresh)
        .file(&format!("blobs/sha256/{config_hex}"), &config)
        // The legacy member is not content-addressed. The stage must skip it.
        .file(&format!("{}/layer.tar", "f".repeat(64)), &fresh)
        .file("index.json", "{}")
        .file("manifest.json", manifest)
        .plain();

    let save = docker::save_from_stream(data_dir, std::io::Cursor::new(export)).unwrap();

    assert!(!staged(data_dir, &cached_hex).exists());
    assert!(staged(data_dir, &fresh_hex).is_file());
    let layers = extract_saved_layers(data_dir, &save);
    assert!(
        cache::layer_dir(data_dir, &layers[0])
            .unwrap()
            .join("fresh")
            .is_file()
    );
    assert!(
        cache::layer_dir(data_dir, &layers[1])
            .unwrap()
            .join("cached")
            .is_file()
    );
    assert!(staging_leftovers(data_dir).is_empty());
}

/// Test that a blob whose content does not match the digest in its name
/// is refused, so that a bad export cannot put wrong data in the cache.
///   1. Make an export with a blob named by a digest of other content
///   2. Stage the export and check the "digest mismatch" error
///   3. Check that the layer cache is empty
#[test]
fn docker_blob_whose_content_does_not_match_its_digest_is_rejected() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let claimed = "a".repeat(64);
    let export = LayerTar::default()
        .file(&format!("blobs/sha256/{claimed}"), "poison")
        .plain();

    let Err(err) = docker::save_from_stream(data_dir, std::io::Cursor::new(export)) else {
        panic!("mismatched docker blob must be rejected");
    };

    assert!(err.to_string().contains("digest mismatch"), "{err}");
    let leftovers: Vec<_> = std::fs::read_dir(cache::layers_root(data_dir).unwrap())
        .unwrap()
        .collect();
    assert!(leftovers.is_empty());
}

/// Test that a `docker save` manifest with a layer name that is not a
/// content-addressed blob is refused, so that the name cannot point out of
/// the layer cache.
///   1. Make an export whose manifest names a layer with `../` parts
///   2. Stage the export and check the "unsupported layer" error
#[test]
fn docker_manifest_layer_with_path_parts_is_rejected() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let config = config_json("", &[]);
    let config_hex = sha256_hex(&config);
    let manifest =
        format!(r#"[{{"Config":"blobs/sha256/{config_hex}","Layers":["../../../etc/passwd"]}}]"#);
    let export = LayerTar::default()
        .file(&format!("blobs/sha256/{config_hex}"), &config)
        .file("manifest.json", manifest)
        .plain();

    let Err(err) = docker::save_from_stream(data_dir, std::io::Cursor::new(export)) else {
        panic!("layer with path parts must be rejected");
    };

    assert!(err.to_string().contains("unsupported layer"), "{err}");
}
