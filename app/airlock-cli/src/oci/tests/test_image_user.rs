use super::*;
use crate::test_cfg::home::TempHome;

fn build(layers: &[String], user: &str) -> anyhow::Result<OciImage> {
    build_oci_image(
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

fn build_err(layers: &[String], user: &str) -> String {
    match build(layers, user) {
        Ok(_) => panic!("USER {user:?} over {layers:?} must not resolve"),
        Err(e) => e.to_string(),
    }
}

#[test]
fn image_user_resolves_through_image_passwd_and_group() {
    let _home = TempHome::new();
    let layers = [passwd_layer("sha256:passwd")];

    for user in [
        "node",
        "1000",
        "node:node",
        "1000:node",
        "node:1000",
        "1000:1000",
    ] {
        let image = build(&layers, user).unwrap();
        assert_eq!((image.uid, image.gid), (1000, 1000), "USER {user:?}");
        assert_eq!(image.container_home, "/home/node", "USER {user:?}");
        assert_eq!(image.user.as_deref(), Some(user));
    }
    let root = build(&layers, "").unwrap();
    assert_eq!(
        (root.uid, root.gid, root.container_home.as_str()),
        (0, 0, "/root")
    );
}

#[test]
fn unknown_image_user_or_group_is_error_not_root() {
    let _home = TempHome::new();
    let layers = [passwd_layer("sha256:passwd")];

    for user in ["ghost", "ghost:node", "node:ghost", "1000:ghost"] {
        assert!(build_err(&layers, user).contains("ghost"), "USER {user:?}");
    }
}

#[test]
fn passwd_symlinked_out_of_layer_is_never_read() {
    let home = TempHome::new();
    let host_passwd = "node:x:1000:1000:Host:/leaked-from-host:/bin/sh\n";
    let host_etc = home.path().join("host-etc");
    std::fs::create_dir_all(&host_etc).unwrap();
    std::fs::write(host_etc.join("passwd"), host_passwd).unwrap();
    let empty_etc = home.path().join("host-etc-empty");
    std::fs::create_dir_all(&empty_etc).unwrap();
    let real = passwd_layer("sha256:real");
    let file_link = cache_layer(
        "sha256:file-link",
        &LayerTar::default()
            .dir("etc")
            .symlink("etc/passwd", host_etc.join("passwd"))
            .gz(),
    );
    let dir_link = cache_layer(
        "sha256:dir-link",
        &LayerTar::default().symlink("etc", &host_etc).gz(),
    );
    let empty_dir_link = cache_layer(
        "sha256:empty-dir-link",
        &LayerTar::default().symlink("etc", &empty_etc).gz(),
    );

    for link in [&file_link, &dir_link] {
        let image = build(&[link.clone(), real.clone()], "node").unwrap();
        assert_eq!(image.container_home, "/home/node");

        let err = build_err(std::slice::from_ref(link), "1000");
        assert!(
            err.contains("no home directory found for uid 1000"),
            "{err}"
        );
        assert!(
            err.contains("etc/passwd") && err.contains("outside the layer"),
            "{err}"
        );
        assert!(!err.contains("leaked-from-host"), "{err}");

        let err = build_err(std::slice::from_ref(link), "node");
        assert!(
            err.contains("no user node found") && err.contains("outside the layer"),
            "{err}"
        );
    }
    let err = build_err(&[empty_dir_link], "1000");
    assert!(err.contains("outside the layer"), "{err}");
}

#[test]
fn passwd_that_cannot_be_read_safely_is_named_in_error() {
    let _home = TempHome::new();
    let mut huge = "#".repeat(usize::try_from(MAX_LAYER_RECORD_FILE).unwrap() + 1);
    huge.push_str("\nnode:x:1000:1000:Node:/home/node:/bin/sh\n");
    let oversized = cache_layer(
        "sha256:oversized",
        &LayerTar::default().file("etc/passwd", huge).gz(),
    );
    let directory = cache_layer(
        "sha256:directory",
        &LayerTar::default().dir("etc/passwd").gz(),
    );
    let dangling = cache_layer(
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
        let err = build_err(&[layer], "1000");
        assert!(err.contains(why), "{err}");
    }
}

#[test]
fn passwd_linked_inside_layer_is_followed() {
    let _home = TempHome::new();
    let merged_usr = cache_layer(
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
        build(&[merged_usr], "1000").unwrap().container_home,
        "/home/node"
    );
}

#[test]
fn legacy_cache_entry_user_is_checked_against_image_passwd() {
    let _home = TempHome::new();
    let mut stored = image("sha256:legacy", vec![passwd_layer("sha256:passwd")]);
    stored.user = None;

    assert_eq!(
        check_legacy_user(&stored, "").unwrap(),
        LegacyUser::Verified
    );
    assert_eq!(
        check_legacy_user(&stored, "root").unwrap(),
        LegacyUser::Verified
    );
    assert_eq!(
        check_legacy_user(&stored, "node").unwrap(),
        LegacyUser::UidMismatch {
            uid: 1000,
            gid: 1000
        }
    );
    stored.uid = 1000;
    assert_eq!(
        check_legacy_user(&stored, "1000").unwrap(),
        LegacyUser::GidOnly { gid: 1000 }
    );
    stored.gid = 1000;
    assert_eq!(
        check_legacy_user(&stored, "1000").unwrap(),
        LegacyUser::Verified
    );
}
