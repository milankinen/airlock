//! Helpers that make sandboxes of both kinds: in the data directory, and in
//! the project directory as older airlock versions made them.

use std::path::{Path, PathBuf};

use airlock_test_utils::block_on_local;

use super::start::fake_image;
use crate::context::Context;
use crate::sandboxes::registry;

/// The `ca.json` of a legacy sandbox: a valid CA, the same in each call.
pub fn legacy_ca() -> &'static str {
    static CA: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    CA.get_or_init(|| {
        use rcgen::{BasicConstraints, CertificateParams, IsCa, KeyPair};
        let mut params = CertificateParams::new(vec![]).unwrap();
        params.is_ca = IsCa::Ca(BasicConstraints::Constrained(0));
        let key = KeyPair::generate().unwrap();
        let cert = params.self_signed(&key).unwrap();
        serde_json::json!({ "cert": cert.pem(), "key": key.serialize_pem() }).to_string()
    })
}

/// The `disk.id` of a legacy sandbox.
pub const LEGACY_DISK_ID: &str = "0123456789abcdef0123456789abcdef";

/// Make a stopped sandbox in `project/.airlock/sandbox` as an older airlock
/// made it: the record files, and a disk image with its identity.
/// Returns:
///   The sandbox directory.
pub fn legacy_sandbox(project: &Path) -> PathBuf {
    let dir = project.join(".airlock/sandbox");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ca.json"), legacy_ca()).unwrap();
    std::fs::write(
        dir.join("run.json"),
        r#"{"last_run":7,"guest_cwd":"/work"}"#,
    )
    .unwrap();
    std::fs::write(dir.join("installs.json"), r#"{"version":1}"#).unwrap();
    std::fs::write(dir.join("disk.img"), b"disk").unwrap();
    std::fs::write(dir.join("disk.id"), format!("{LEGACY_DISK_ID}\n")).unwrap();
    dir
}

/// Write a complete cache file of the [`fake_image`] `name` in the data
/// directory of `context`.
/// Returns:
///   The image digest and the content of the cache file.
pub fn cached_image(context: &Context, name: &str) -> (String, String) {
    let image = fake_image(name);
    let mut json = serde_json::to_value(&image).unwrap();
    json["schema"] = "v2".into();
    let text = json.to_string();
    let path = crate::cache::image_path(&context.data, &image.image_id).unwrap();
    std::fs::write(path.to_path().unwrap(), &text).unwrap();
    (image.image_id, text)
}

/// Make the directory `dir` of a stopped sandbox, with the lock file that a
/// run leaves.
///
/// Takes no lock. A lock that the test takes and releases can stay held for
/// a short time, because a `fork` in a parallel test copies the open lock
/// file.
pub fn stopped_sandbox_dir(dir: &Path) {
    std::fs::create_dir_all(dir).unwrap();
    std::fs::write(dir.join("lock"), b"").unwrap();
}

/// Make a stopped sandbox of the project `project` in the data directory of
/// `context`.
/// Returns:
///   The registry id and the sandbox directory.
pub fn data_dir_sandbox(context: &Context, project: &Path) -> (String, PathBuf) {
    let boxes = &context.data.sandboxes;
    let id = block_on_local(registry::find_or_register(
        &context.db,
        boxes,
        &super::oci::verified_dir(project),
    ))
    .unwrap();
    let dir = context.data.sandboxes.given().join(&id);
    stopped_sandbox_dir(&dir);
    (id, dir)
}
