use std::path::{Path, PathBuf};

use crate::config::config::{MissingAction, Mount};
use crate::oci::{MountType, resolve_mounts};

fn mount(source: &str, target: &str) -> Mount {
    Mount {
        enabled: true,
        source: source.into(),
        target: target.into(),
        read_only: false,
        missing: MissingAction::Fail,
    }
}

fn mount_with(source: &str, target: &str, missing: MissingAction) -> Mount {
    Mount {
        enabled: true,
        source: source.into(),
        target: target.into(),
        read_only: false,
        missing,
    }
}

#[test]
fn absolute_existing_dir() {
    let tmp = tempdir();
    let src = tmp.join("mydir");
    std::fs::create_dir(&src).unwrap();

    let mounts = resolve_mounts(
        &[mount(&src.to_string_lossy(), "/container/data")],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].target, "/container/data");
    assert!(matches!(mounts[0].mount_type, MountType::Dir { .. }));
}

#[test]
fn relative_path_resolved_against_cwd() {
    let tmp = tempdir();
    let src = tmp.join("rel-dir");
    std::fs::create_dir(&src).unwrap();

    let mounts = resolve_mounts(
        &[mount("rel-dir", "/target")],
        Path::new("/home/test"),
        "/root",
        &tmp, // cwd
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    // Source should be the absolute resolved path
    assert_eq!(mounts[0].source, std::fs::canonicalize(&src).unwrap());
}

#[test]
fn dot_slash_relative_path() {
    let tmp = tempdir();
    let src = tmp.join("dot-dir");
    std::fs::create_dir(&src).unwrap();

    let mounts = resolve_mounts(
        &[mount("./dot-dir", "/target")],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].source, std::fs::canonicalize(&src).unwrap());
}

#[test]
fn missing_fail_is_default() {
    let tmp = tempdir();

    let result = resolve_mounts(
        &[mount("/nonexistent/path", "/target")],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    );

    assert!(result.is_err());
    let err = result.unwrap_err().to_string();
    assert!(
        err.contains("does not exist"),
        "expected 'does not exist' error, got: {err}"
    );
}

#[test]
fn missing_ignore_skips_silently() {
    let tmp = tempdir();

    let mounts = resolve_mounts(
        &[mount_with("/nonexistent", "/target", MissingAction::Ignore)],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert!(mounts.is_empty(), "ignored mount should be skipped");
}

#[test]
fn missing_warn_skips() {
    let tmp = tempdir();

    let mounts = resolve_mounts(
        &[mount_with("/nonexistent", "/target", MissingAction::Warn)],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert!(mounts.is_empty(), "warned mount should be skipped");
}

#[test]
fn missing_create_makes_directory() {
    let tmp = tempdir();
    let src = tmp.join("auto-created");
    assert!(!src.exists());

    let mounts = resolve_mounts(
        &[mount_with(
            &src.to_string_lossy(),
            "/target",
            MissingAction::Create,
        )],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    assert!(src.exists(), "directory should have been created");
    assert!(src.is_dir());
}

#[test]
fn missing_create_nested() {
    let tmp = tempdir();
    let src = tmp.join("a/b/c/deep");
    assert!(!src.exists());

    let mounts = resolve_mounts(
        &[mount_with(
            &src.to_string_lossy(),
            "/target",
            MissingAction::Create,
        )],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    assert!(src.exists(), "nested directory should have been created");
}

#[test]
fn missing_create_relative() {
    let tmp = tempdir();

    let mounts = resolve_mounts(
        &[mount_with(
            "new-relative-dir",
            "/target",
            MissingAction::Create,
        )],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    let expected = tmp.join("new-relative-dir");
    assert!(
        expected.exists(),
        "relative dir should be created under cwd"
    );
}

#[test]
fn tilde_expansion_source() {
    let tmp = tempdir();
    let home = tmp.join("fakehome");
    let src = home.join(".config");
    std::fs::create_dir_all(&src).unwrap();

    let mounts = resolve_mounts(
        &[mount("~/.config", "/config")],
        &home,
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].source, std::fs::canonicalize(&src).unwrap());
}

#[test]
fn tilde_expansion_target() {
    let tmp = tempdir();
    let src = tmp.join("data");
    std::fs::create_dir(&src).unwrap();

    let mounts = resolve_mounts(
        &[mount(&src.to_string_lossy(), "~/data")],
        Path::new("/home/test"),
        "/home/container",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].target, "/home/container/data");
}

#[test]
fn relative_target_resolved_against_guest_cwd() {
    let tmp = tempdir();
    let src = tmp.join("myfile.txt");
    std::fs::write(&src, "content").unwrap();

    let mounts = resolve_mounts(
        &[mount(&src.to_string_lossy(), "myfile.txt")],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].target, "/workdir/myfile.txt");
}

#[test]
fn file_mount() {
    let tmp = tempdir();
    let src = tmp.join("myfile.txt");
    std::fs::write(&src, "content").unwrap();

    let mounts = resolve_mounts(
        &[mount(&src.to_string_lossy(), "/container/file.txt")],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    assert!(matches!(mounts[0].mount_type, MountType::File { .. }));
}

#[test]
fn multiple_mounts_mixed() {
    let tmp = tempdir();
    let dir1 = tmp.join("dir1");
    std::fs::create_dir(&dir1).unwrap();
    let dir2 = tmp.join("dir2");
    std::fs::create_dir(&dir2).unwrap();

    let mounts = resolve_mounts(
        &[
            mount(&dir1.to_string_lossy(), "/a"),
            mount_with("/nonexistent", "/b", MissingAction::Ignore),
            mount(&dir2.to_string_lossy(), "/c"),
        ],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 2, "ignored mount should be skipped");
    assert_eq!(mounts[0].target, "/a");
    assert_eq!(mounts[1].target, "/c");
}

#[test]
fn read_only_preserved() {
    let tmp = tempdir();
    let src = tmp.join("ro-dir");
    std::fs::create_dir(&src).unwrap();

    let mounts = resolve_mounts(
        &[Mount {
            enabled: true,
            source: src.to_string_lossy().into(),
            target: "/ro".into(),
            read_only: true,
            missing: MissingAction::Fail,
        }],
        Path::new("/home/test"),
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert!(mounts[0].read_only);
}

#[test]
fn tilde_missing_fail() {
    let tmp = tempdir();
    let home = tmp.join("fakehome");
    std::fs::create_dir(&home).unwrap();
    // ~/.nonexistent doesn't exist → fail
    let result = resolve_mounts(
        &[mount("~/.nonexistent", "/target")],
        &home,
        "/root",
        &tmp,
        Path::new("/workdir"),
    );
    assert!(result.is_err());
}

#[test]
fn tilde_missing_create() {
    let tmp = tempdir();
    let home = tmp.join("fakehome");
    std::fs::create_dir(&home).unwrap();

    let mounts = resolve_mounts(
        &[mount_with(
            "~/.auto-created",
            "/target",
            MissingAction::Create,
        )],
        &home,
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    let expected = home.join(".auto-created");
    assert!(expected.exists(), "should create under home dir");
    assert!(expected.is_dir());
}

#[test]
fn tilde_missing_ignore() {
    let tmp = tempdir();
    let home = tmp.join("fakehome");
    std::fs::create_dir(&home).unwrap();

    let mounts = resolve_mounts(
        &[mount_with("~/.nope", "/target", MissingAction::Ignore)],
        &home,
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert!(mounts.is_empty());
    assert!(!home.join(".nope").exists(), "should not create anything");
}

#[test]
fn bare_tilde_source() {
    let tmp = tempdir();
    let home = tmp.join("fakehome");
    std::fs::create_dir(&home).unwrap();

    let mounts = resolve_mounts(
        &[mount("~", "~")],
        &home,
        "/root",
        &tmp,
        Path::new("/workdir"),
    )
    .unwrap();

    assert_eq!(mounts.len(), 1);
    assert_eq!(mounts[0].source, std::fs::canonicalize(&home).unwrap());
    assert_eq!(mounts[0].target, "/root");
}

/// Create a temporary directory that's cleaned up on drop.
fn tempdir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("ezpez-test-{}", std::process::id()));
    let dir = dir.join(format!(
        "{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}
