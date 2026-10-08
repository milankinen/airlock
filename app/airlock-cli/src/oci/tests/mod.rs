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
fn cache_layer(digest: &str, tar: &[u8]) -> String {
    layer::ensure_layer_cached(digest, |dest| Ok(std::fs::write(dest, tar)?), None).unwrap();
    cache::layer_key(digest)
}

/// A layer whose `/etc/passwd` and `/etc/group` declare root and one
/// unprivileged user, as `node`, `python` and `debian` derived images do.
fn passwd_layer(digest: &str) -> String {
    cache_layer(
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

/// An image config as the registry or `docker save` hands it over.
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
