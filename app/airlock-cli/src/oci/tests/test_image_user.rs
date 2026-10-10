//! Tests for the resolution of the image `USER` to uid, gid and home
//! through the passwd and group files in the image layers.

use airlock_test_utils::temp_dir;

use super::*;

/// Build an image over `layers` (topmost first) with the `USER` value
/// `user`.
fn build(data_dir: &Path, layers: &[String], user: &str) -> anyhow::Result<OciImage> {
    build_oci_image(
        data_dir,
        "sha256:img".into(),
        "node:22".into(),
        layers.to_vec(),
        &image_config(serde_json::json!({
            "architecture": "amd64",
            "os": "linux",
            "config": { "User": user },
            "rootfs": { "type": "layers", "diff_ids": [] },
        })),
    )
}

/// Build an image as [`build`] does and return the error message. Panics
/// if the build succeeds.
fn build_err(data_dir: &Path, layers: &[String], user: &str) -> String {
    match build(data_dir, layers, user) {
        Ok(_) => panic!("USER {user:?} over {layers:?} must not resolve"),
        Err(e) => e.to_string(),
    }
}

/// Test that each form of `USER` (name or number, with or without group)
/// resolves through the passwd and group files of the image.
///   1. Cache a layer that declares root and the `node` user and group
///   2. Build an image for each `USER` form of `node`
///   3. Check the uid, gid, home and stored `USER` of each image
///   4. Check that an empty `USER` gives root
#[test]
fn image_user_resolves_through_image_passwd_and_group() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let layers = [passwd_layer(data_dir, "sha256:passwd")];

    for user in [
        "node",
        "1000",
        "node:node",
        "1000:node",
        "node:1000",
        "1000:1000",
    ] {
        let image = build(data_dir, &layers, user).unwrap();
        assert_eq!((image.uid, image.gid), (1000, 1000), "USER {user:?}");
        assert_eq!(image.container_home, "/home/node", "USER {user:?}");
        assert_eq!(image.user.as_deref(), Some(user));
    }
    let root = build(data_dir, &layers, "").unwrap();
    assert_eq!(
        (root.uid, root.gid, root.container_home.as_str()),
        (0, 0, "/root")
    );
}

/// Test that an unknown user or group is an error, not a silent fallback to
/// root.
///   1. Cache a layer that declares root and the `node` user
///   2. Build images with an unknown user or group in each position
///   3. Check that each error names the unknown value
#[test]
fn unknown_image_user_or_group_is_error_not_root() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let layers = [passwd_layer(data_dir, "sha256:passwd")];

    for user in ["ghost", "ghost:node", "node:ghost", "1000:ghost"] {
        assert!(
            build_err(data_dir, &layers, user).contains("ghost"),
            "USER {user:?}"
        );
    }
}

/// Test that a passwd file or `etc` directory that links out of its layer is
/// never read, so that an image cannot read host files.
///   1. Write a passwd file on the host and make an empty host directory
///   2. Cache layers whose `etc/passwd` or `etc` links to them
///   3. Check that a lower layer with a real passwd still resolves the user
///   4. Check that a link layer alone gives an error that says "outside the
///      layer" and has no host data
///   5. Check that a link to an empty host directory also says "outside the
///      layer"
#[test]
fn passwd_symlinked_out_of_layer_is_never_read() {
    let home = temp_dir();
    let data_dir = home.path();
    let host_passwd = "node:x:1000:1000:Host:/leaked-from-host:/bin/sh\n";
    let host_etc = home.path().join("host-etc");
    std::fs::create_dir_all(&host_etc).unwrap();
    std::fs::write(host_etc.join("passwd"), host_passwd).unwrap();
    let empty_etc = home.path().join("host-etc-empty");
    std::fs::create_dir_all(&empty_etc).unwrap();
    let real = passwd_layer(data_dir, "sha256:real");
    let file_link = cache_layer(
        data_dir,
        "sha256:file-link",
        &LayerTar::default()
            .dir("etc")
            .symlink("etc/passwd", host_etc.join("passwd"))
            .gz(),
    );
    let dir_link = cache_layer(
        data_dir,
        "sha256:dir-link",
        &LayerTar::default().symlink("etc", &host_etc).gz(),
    );
    let empty_dir_link = cache_layer(
        data_dir,
        "sha256:empty-dir-link",
        &LayerTar::default().symlink("etc", &empty_etc).gz(),
    );

    for link in [&file_link, &dir_link] {
        // The link layer is skipped and the real layer below it is used.
        let image = build(data_dir, &[link.clone(), real.clone()], "node").unwrap();
        assert_eq!(image.container_home, "/home/node");

        let err = build_err(data_dir, std::slice::from_ref(link), "1000");
        assert!(
            err.contains("no home directory found for uid 1000"),
            "{err}"
        );
        assert!(
            err.contains("etc/passwd") && err.contains("outside the layer"),
            "{err}"
        );
        assert!(!err.contains("leaked-from-host"), "{err}");

        let err = build_err(data_dir, std::slice::from_ref(link), "node");
        assert!(
            err.contains("no user node found") && err.contains("outside the layer"),
            "{err}"
        );
    }
    let err = build_err(data_dir, &[empty_dir_link], "1000");
    assert!(err.contains("outside the layer"), "{err}");
}

/// Test that a passwd file that is not safe to read is named in the error
/// with the reason.
///   1. Cache layers with an oversized passwd file, a passwd directory and
///      a passwd symlink to a missing file
///   2. Build an image over each layer
///   3. Check that each error gives the correct reason
#[test]
fn passwd_that_cannot_be_read_safely_is_named_in_error() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let mut huge = "#".repeat(usize::try_from(MAX_LAYER_RECORD_FILE).unwrap() + 1);
    huge.push_str("\nnode:x:1000:1000:Node:/home/node:/bin/sh\n");
    let oversized = cache_layer(
        data_dir,
        "sha256:oversized",
        &LayerTar::default().file("etc/passwd", huge).gz(),
    );
    let directory = cache_layer(
        data_dir,
        "sha256:directory",
        &LayerTar::default().dir("etc/passwd").gz(),
    );
    let dangling = cache_layer(
        data_dir,
        "sha256:dangling",
        &LayerTar::default()
            .dir("etc")
            .symlink("etc/passwd", "../usr/lib/passwd")
            .gz(),
    );

    for (layer, why) in [
        (oversized, "larger than"),
        (directory, "not a regular file"),
        (dangling, "cannot be resolved"),
    ] {
        let err = build_err(data_dir, &[layer], "1000");
        assert!(err.contains(why), "{err}");
    }
}

/// Test that a passwd symlink to a file in the same layer is followed, as
/// in images with a merged `/usr`.
///   1. Cache a layer whose `etc/passwd` links to `usr/lib/passwd`
///   2. Check that uid 1000 resolves to its home directory
#[test]
fn passwd_linked_inside_layer_is_followed() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let merged_usr = cache_layer(
        data_dir,
        "sha256:merged-usr",
        &LayerTar::default()
            .file(
                "usr/lib/passwd",
                "node:x:1000:1000:Node:/home/node:/bin/sh\n",
            )
            .dir("etc")
            .symlink("etc/passwd", "../usr/lib/passwd")
            .gz(),
    );

    assert_eq!(
        build(data_dir, &[merged_usr], "1000")
            .unwrap()
            .container_home,
        "/home/node"
    );
}

/// Test that the uid and gid of a cache entry without a `user` field are
/// checked against the image passwd, so that airlock finds a sandbox that
/// ran as the wrong user.
///   1. Make a legacy entry with uid and gid 0
///   2. Check that an empty `USER` and `root` are verified
///   3. Check that `node` gives a uid mismatch
///   4. Fix the uid and check that only the gid differs
///   5. Fix the gid and check that the entry is verified
#[test]
fn legacy_cache_entry_user_is_checked_against_image_passwd() {
    let tmp = temp_dir();
    let data_dir = tmp.path();
    let mut stored = image(
        "sha256:legacy",
        vec![passwd_layer(data_dir, "sha256:passwd")],
    );
    stored.user = None;

    assert_eq!(
        check_legacy_user(data_dir, &stored, "").unwrap(),
        LegacyUser::Verified
    );
    assert_eq!(
        check_legacy_user(data_dir, &stored, "root").unwrap(),
        LegacyUser::Verified
    );
    assert_eq!(
        check_legacy_user(data_dir, &stored, "node").unwrap(),
        LegacyUser::UidMismatch {
            uid: 1000,
            gid: 1000
        }
    );
    stored.uid = 1000;
    assert_eq!(
        check_legacy_user(data_dir, &stored, "1000").unwrap(),
        LegacyUser::GidOnly { gid: 1000 }
    );
    stored.gid = 1000;
    assert_eq!(
        check_legacy_user(data_dir, &stored, "1000").unwrap(),
        LegacyUser::Verified
    );
}
