//! Tests of the image link that a new sandbox gets from its template.

use std::os::unix::fs::MetadataExt;

use crate::templates::{IMAGE_FILE, Template, TemplateConfig, seed};
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::temp_dir;
use crate::util::PinnedDir;
use crate::vm::disk::DISK_FILE;

/// Test that a symlink at the `image` file of a template does not give the
/// new sandbox a hard link to the link target. On macOS a hard link of a
/// symlink links its target, which can be any host file.
///   1. Make a template with a disk and an `image` symlink to a host file
///   2. Copy the template into a new sandbox
///   3. Check that the sandbox has the disk but no `image`, and that the
///      host file has no new link
#[test]
fn template_image_symlink_is_not_linked_into_sandbox() {
    let root = temp_dir();
    let template_dir = root.path().join("template");
    let sandbox_dir = root.path().join("sandbox");
    let host = root.path().join("host-file");
    std::fs::create_dir_all(&template_dir).unwrap();
    std::fs::create_dir_all(&sandbox_dir).unwrap();
    std::fs::write(&host, b"host").unwrap();
    std::fs::write(template_dir.join(DISK_FILE), b"disk").unwrap();
    std::os::unix::fs::symlink(&host, template_dir.join(IMAGE_FILE)).unwrap();
    let template = Template {
        name: "base".into(),
        dir: verified_dir(&template_dir),
        config: TemplateConfig::parse("").unwrap(),
    };

    let sandbox = PinnedDir::pin(&sandbox_dir).unwrap();
    seed::seed(&template, &sandbox, &verified_dir(&sandbox_dir)).unwrap();

    assert!(sandbox_dir.join(DISK_FILE).is_file());
    assert!(std::fs::symlink_metadata(sandbox_dir.join(IMAGE_FILE)).is_err());
    assert_eq!(std::fs::metadata(&host).unwrap().nlink(), 1);
}
