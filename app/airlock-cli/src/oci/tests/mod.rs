//! Tests for the OCI image cache: layer extraction, local image import,
//! image users, `os-release` and the garbage collector.

mod test_docker_save;
mod test_image_cache;
mod test_image_user;
#[cfg(target_os = "linux")]
mod test_layer_cache;
mod test_os_release;

use super::*;
use crate::test_cfg::oci::LayerTar;

/// Extract `tar` into the layer cache as the layer `digest`, the way a
/// registry pull does, and return its layer key.
fn cache_layer(data_dir: &Path, digest: &str, tar: &[u8]) -> String {
    layer::ensure_layer_cached(
        data_dir,
        digest,
        |dest| Ok(std::fs::write(dest, tar)?),
        None,
    )
    .unwrap();
    cache::layer_key(digest)
}

/// Cache a layer whose `/etc/passwd` and `/etc/group` declare root and one
/// normal user (`node`, uid 1000), as images such as `node` have.
/// Returns:
///   The layer key.
fn passwd_layer(data_dir: &Path, digest: &str) -> String {
    cache_layer(
        data_dir,
        digest,
        &LayerTar::default()
            .file(
                "etc/passwd",
                "root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1000:Node:/home/node:/bin/sh\n",
            )
            .file("etc/group", "root:x:0:\nnode:x:1000:\n")
            .gz(),
    )
}

/// Parse `json` as an image config, in the form that a registry or
/// `docker save` gives.
fn image_config(json: serde_json::Value) -> OciConfig {
    serde_json::from_value(json).unwrap()
}

/// A cached image entry over `layers` (topmost first).
fn image(digest: &str, layers: Vec<String>) -> OciImage {
    OciImage {
        image_id: digest.to_string(),
        name: "alpine:3.19".to_string(),
        image_layers: layers,
        container_home: "/root".to_string(),
        uid: 0,
        gid: 0,
        cmd: vec!["/bin/sh".to_string()],
        env: vec![],
        user: Some(String::new()),
    }
}
