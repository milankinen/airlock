//! Tests for the self-update flow against a local release server.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;

use axum::Router;
use axum::http::StatusCode;
use axum::response::Redirect;
use axum::routing::get;
use indicatif::ProgressBar;
use sha2::{Digest, Sha256};

use crate::self_update::{self, ReleaseSource, Version};
use crate::test_cfg::{serve, temp_dir};

const LATEST: &str = "v2099.1.0";

/// Make a release archive with an `airlock` script that prints `version`
/// for `-V`, as a release binary does.
fn release_archive(version: &str) -> Vec<u8> {
    let script = format!("#!/bin/sh\necho 'airlock {version} (0123456)'\n");
    let mut header = tar::Header::new_gnu();
    header.set_size(script.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    let mut tar = tar::Builder::new(flate2::write::GzEncoder::new(
        Vec::new(),
        flate2::Compression::default(),
    ));
    tar.append_data(&mut header, "airlock", script.as_bytes())
        .unwrap();
    tar.into_inner().unwrap().finish().unwrap()
}

/// Serve a fake GitHub release `LATEST` with `archive` and `checksum`. The
/// latest release URL redirects to the release tag page, as on GitHub.
async fn serve_release(archive: Vec<u8>, checksum: String) -> ReleaseSource {
    let name = self_update::archive_name(&Version::parse(LATEST).unwrap()).unwrap();
    let app = Router::new()
        .route(
            "/latest",
            get(|| async { Redirect::temporary(&format!("/releases/tag/{LATEST}")) }),
        )
        .route(
            &format!("/releases/tag/{LATEST}"),
            get(|| async { "release page" }),
        )
        .route(
            &format!("/download/{LATEST}/{name}"),
            get(move || async move { archive }),
        )
        .route(
            &format!("/download/{LATEST}/{name}.sha256"),
            get(move || async move { format!("{checksum}  {name}\n") }),
        )
        .fallback(|| async { StatusCode::NOT_FOUND });
    let addr = serve(app).await;
    ReleaseSource {
        latest_url: format!("http://{addr}/latest"),
        download_url: format!("http://{addr}/download"),
    }
}

/// Make an installed old airlock binary in `dir`.
fn old_binary(dir: &Path) -> std::path::PathBuf {
    let path = dir.join("airlock");
    std::fs::write(&path, "old binary").unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    path
}

/// Return the names of the files in `dir`.
fn file_names(dir: &Path) -> Vec<String> {
    std::fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect()
}

/// Test that the update finds the latest release and replaces the
/// installed binary with the binary from the release archive.
///   1. Serve a release with a correct checksum
///   2. Get the latest version and install it over an old binary
///   3. Check that the binary is new and executable, and that no
///      temporary files stay
#[tokio::test]
async fn update_to_latest_release_replaces_binary() {
    let archive = release_archive(LATEST);
    let checksum = hex::encode(Sha256::digest(&archive));
    let source = serve_release(archive, checksum).await;
    let dir = temp_dir();
    let target = old_binary(dir.path());
    let client = self_update::http_client(&Version::parse("v2026.1.0").unwrap()).unwrap();

    let latest = self_update::latest_version(&client, &source).await.unwrap();
    assert_eq!(latest.to_string(), LATEST);
    self_update::install(&client, &source, &latest, &target, &ProgressBar::hidden())
        .await
        .unwrap();

    let installed = std::fs::read_to_string(&target).unwrap();
    assert!(
        installed.contains(&format!("airlock {LATEST}")),
        "{installed}"
    );
    let mode = std::fs::metadata(&target).unwrap().permissions().mode();
    assert_eq!(mode & 0o111, 0o111);
    assert_eq!(file_names(dir.path()), ["airlock"]);
}

/// Test that the update refuses an archive that does not match the release
/// checksum, so that a changed download cannot replace the binary.
///   1. Serve a release with a wrong checksum
///   2. Try to install it over an old binary
///   3. Check that the install fails and the old binary stays
#[tokio::test]
async fn update_with_checksum_mismatch_keeps_old_binary() {
    let source = serve_release(release_archive(LATEST), "00".repeat(32)).await;
    let dir = temp_dir();
    let target = old_binary(dir.path());
    let client = self_update::http_client(&Version::parse("v2026.1.0").unwrap()).unwrap();

    let latest = Version::parse(LATEST).unwrap();
    let err = self_update::install(&client, &source, &latest, &target, &ProgressBar::hidden())
        .await
        .unwrap_err();

    assert!(format!("{err:#}").contains("checksum mismatch"), "{err:#}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old binary");
    assert_eq!(file_names(dir.path()), ["airlock"]);
}

/// Test that the update refuses a binary that reports a different version,
/// so that a broken or wrong release does not replace the binary.
///   1. Serve a release whose binary reports an older version
///   2. Try to install it over an old binary
///   3. Check that the install fails and the old binary stays
#[tokio::test]
async fn update_with_wrong_binary_version_keeps_old_binary() {
    let archive = release_archive("v2026.1.0");
    let checksum = hex::encode(Sha256::digest(&archive));
    let source = serve_release(archive, checksum).await;
    let dir = temp_dir();
    let target = old_binary(dir.path());
    let client = self_update::http_client(&Version::parse("v2026.1.0").unwrap()).unwrap();

    let latest = Version::parse(LATEST).unwrap();
    let err = self_update::install(&client, &source, &latest, &target, &ProgressBar::hidden())
        .await
        .unwrap_err();

    assert!(format!("{err:#}").contains("new binary reports"), "{err:#}");
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "old binary");
    assert_eq!(file_names(dir.path()), ["airlock"]);
}
