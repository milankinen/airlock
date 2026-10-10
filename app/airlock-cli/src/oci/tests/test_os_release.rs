//! Tests for the detection of the image distribution from `os-release`.

use airlock_test_utils::temp_dir;

use super::*;

/// The `os-release` identity of an image over `layers` (topmost first).
fn os_release_of(data_dir: &Path, layers: Vec<String>) -> Option<OsRelease> {
    os_release(data_dir, &image("sha256:img", layers))
}

/// Test that `os-release` comes from the topmost layer with a safe and
/// non-empty file, so that airlock detects the correct distribution.
///   1. Stack an empty file, a symlink out of the layer and an unrelated
///      layer over an Ubuntu layer, and check that Ubuntu is found
///   2. Put a layer with an in-layer symlink to `usr/lib/os-release` on top
///      and check its lowercase ID and ID_LIKE list
///   3. Check that a file with no `ID` gives no result
#[test]
fn os_release_comes_from_topmost_layer_that_has_it_safely() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let ubuntu = cache_layer(
        data_dir,
        "sha256:ubuntu",
        &LayerTar::default()
            .file(
                "etc/os-release",
                "NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n",
            )
            .gz(),
    );
    let escaping = cache_layer(
        data_dir,
        "sha256:escaping",
        &LayerTar::default()
            .dir("etc")
            .symlink("etc/os-release", "/etc/hostname")
            .gz(),
    );
    let emptied = cache_layer(
        data_dir,
        "sha256:emptied",
        &LayerTar::default().file("etc/os-release", "").gz(),
    );
    let unrelated = cache_layer(
        data_dir,
        "sha256:unrelated",
        &LayerTar::default().file("app/main.js", "").gz(),
    );

    // An empty file is a whiteout in the layer cache, so the search goes on
    // to the next layer.
    let release =
        os_release_of(data_dir, vec![emptied, escaping, unrelated, ubuntu.clone()]).unwrap();
    assert_eq!(
        release,
        OsRelease {
            id: "ubuntu".into(),
            id_like: vec!["debian".into()],
        }
    );

    let rocky = cache_layer(
        data_dir,
        "sha256:rocky",
        &LayerTar::default()
            .file(
                "usr/lib/os-release",
                "ID=\"Rocky\"\nID_LIKE=\"rhel centos fedora\"\n",
            )
            .dir("etc")
            .symlink("etc/os-release", "../usr/lib/os-release")
            .gz(),
    );
    let release = os_release_of(data_dir, vec![rocky, ubuntu]).unwrap();
    assert_eq!(release.id, "rocky");
    assert_eq!(release.id_like, ["rhel", "centos", "fedora"]);

    let nameless = cache_layer(
        data_dir,
        "sha256:nameless",
        &LayerTar::default().file("etc/os-release", "NAME=x\n").gz(),
    );
    assert!(os_release_of(data_dir, vec![nameless]).is_none());
}
