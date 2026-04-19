//! Project CA injection. Builds a tmpfs lowerdir (`/mnt/ca-overlay`)
//! containing per-bundle copies of every CA bundle the image ships,
//! each with the project CA appended. `overlay.rs` splices the tmpfs
//! on top of the image layers in the overlayfs `lowerdir` stack —
//! writes never land on the persistent upperdir, so the appended CA
//! doesn't accumulate across reboots.
//!
//! Also drops the raw CA at every well-known anchor path so distro
//! trust-update tools (`update-ca-certificates`, `update-ca-trust`,
//! `trust extract-compat`) regenerate bundles that still include it.

use std::path::Path;

use tracing::debug;

use crate::init::MountConfig;

/// CA bundle paths known across common distros. Each path is relative to the
/// rootfs. Guest init merges the project CA into each existing bundle (read
/// from the image's lower layers) and falls back to writing the Debian/Ubuntu
/// path when none are present, so `SSL_CERT_FILE` can point at a predictable
/// location in minimal images.
const BUNDLE_PATHS: &[&str] = &[
    "etc/ssl/certs/ca-certificates.crt", // Debian/Ubuntu/Alpine
    "etc/ssl/cert.pem",                  // Alpine/LibreSSL
    "etc/pki/tls/certs/ca-bundle.crt",   // RHEL/CentOS/Fedora
    "etc/ssl/ca-bundle.pem",             // openSUSE/SLES
    "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem", // RHEL/Fedora
];

/// Drop-in anchor locations for distro trust-update tools. When the user or
/// a package postinst runs `update-ca-certificates` / `update-ca-trust` /
/// `trust extract-compat` it rebuilds the bundles from these directories —
/// so shipping the project CA as a plain file here makes it survive any
/// future rebuild of `etc/ssl/certs/ca-certificates.crt` on the upperdir.
const ANCHOR_PATHS: &[&str] = &[
    "usr/local/share/ca-certificates/airlock.crt", // Debian/Ubuntu/Alpine — update-ca-certificates
    "etc/pki/ca-trust/source/anchors/airlock.crt", // RHEL/Fedora/CentOS — update-ca-trust
    "etc/pki/trust/anchors/airlock.crt",           // openSUSE/SLES — update-ca-certificates
    "etc/ca-certificates/trust-source/anchors/airlock.crt", // Arch — trust extract-compat
];

/// tmpfs lowerdir holding pre-merged CA bundles. Placed above the image
/// layers in the overlayfs stack so the project CA is visible without any
/// write ever landing on the persistent upperdir.
const OVERLAY_DIR: &str = "/mnt/ca-overlay";

/// Build a tmpfs lowerdir containing per-bundle copies of every CA bundle the
/// image ships, each with the project CA appended. Returns the tmpfs path
/// when anything was written (so the caller can splice it into `lowerdir`),
/// or `None` when there's no project CA to inject.
///
/// This runs **before** overlayfs is mounted: for each well-known bundle path
/// we walk `image_layers` topmost-first, take the first layer that ships a
/// non-empty copy of that file, append the project CA, and drop the result
/// into the tmpfs at the same relative path. Doing the merge against the
/// pristine layer content — not the already-merged overlayfs view — is what
/// prevents the CA from accumulating across reboots when the upperdir is
/// persisted on the project disk.
pub(super) fn prepare_overlay(mounts: &MountConfig) -> anyhow::Result<Option<&'static str>> {
    if mounts.ca_cert.is_empty() {
        return Ok(None);
    }
    std::fs::create_dir_all(OVERLAY_DIR)?;
    super::mount::fs(
        "ca-overlay",
        OVERLAY_DIR,
        "tmpfs",
        libc::MS_NOSUID | libc::MS_NODEV,
        "mode=0755",
    )?;

    let mut wrote_any = false;
    for rel in BUNDLE_PATHS {
        let Some(base) = find_bundle_in_layers(&mounts.image_layers, rel)? else {
            continue;
        };
        write_bundle(rel, &base, &mounts.ca_cert)?;
        wrote_any = true;
        debug!("ca: merged /{rel} from image layers");
    }
    if !wrote_any {
        write_bundle(BUNDLE_PATHS[0], &[], &mounts.ca_cert)?;
        debug!(
            "ca: wrote fallback /{} (no CA bundle shipped by image)",
            BUNDLE_PATHS[0]
        );
    }

    // Drop the raw CA into every well-known anchor directory so trust-update
    // tools regenerate bundles that still include it. Cheap and harmless when
    // the tool isn't installed — the file just sits there unread.
    for rel in ANCHOR_PATHS {
        write_bundle(rel, &[], &mounts.ca_cert)?;
        debug!("ca: dropped anchor /{rel}");
    }
    Ok(Some(OVERLAY_DIR))
}

/// Find the first layer that ships `rel` (topmost-first) and return its
/// contents. An empty file in a layer is treated as "masked here" — either an
/// overlayfs whiteout placeholder from our extractor or a deliberately empty
/// bundle — and stops the walk so we don't resurrect content the image meant
/// to hide. `None` means no layer had the path at all.
fn find_bundle_in_layers(layers: &[String], rel: &str) -> anyhow::Result<Option<Vec<u8>>> {
    for digest in layers {
        let path = Path::new("/mnt/layers").join(digest).join(rel);
        match std::fs::symlink_metadata(&path) {
            Ok(meta) if meta.file_type().is_file() && meta.len() > 0 => {
                return Ok(Some(std::fs::read(&path)?));
            }
            Ok(_) => return Ok(None),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => return Err(e.into()),
        }
    }
    Ok(None)
}

fn write_bundle(rel: &str, base: &[u8], ca_cert: &[u8]) -> anyhow::Result<()> {
    let target = Path::new(OVERLAY_DIR).join(rel);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut out = base.to_vec();
    if !out.is_empty() && !out.ends_with(b"\n") {
        out.push(b'\n');
    }
    out.extend_from_slice(ca_cert);
    std::fs::write(&target, &out)
        .map_err(|e| anyhow::anyhow!("write CA bundle {}: {e}", target.display()))
}
