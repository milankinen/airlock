//! Project CA in the container.
//!
//! Makes TLS clients in the container trust the project CA. The CA bundles of
//! the container include the project CA, also after a trust update tool makes
//! the bundles again.

use std::path::Path;

use tracing::debug;

use crate::init::MountConfig;

/// CA bundle paths of common distros, relative to the rootfs.
///
/// Guest init adds the project CA to each bundle that the image layers have.
/// If the image has none, guest init writes the Debian/Ubuntu path (the
/// first item). Thus `SSL_CERT_FILE` can point to a known location also in
/// minimal images.
const BUNDLE_PATHS: &[&str] = &[
    "etc/ssl/certs/ca-certificates.crt", // Debian/Ubuntu/Alpine
    "etc/ssl/cert.pem",                  // Alpine/LibreSSL
    "etc/pki/tls/certs/ca-bundle.crt",   // RHEL/CentOS/Fedora
    "etc/ssl/ca-bundle.pem",             // openSUSE/SLES
    "etc/pki/ca-trust/extracted/pem/tls-ca-bundle.pem", // RHEL/Fedora
];

/// Anchor file locations for distro trust-update tools.
///
/// The user or a package postinst script can run `update-ca-certificates`,
/// `update-ca-trust` or `trust extract-compat`. These tools make the
/// bundles again from these directories. With the project CA as a plain
/// file here, the CA stays in all bundles that these tools make again on
/// the upperdir (for example `etc/ssl/certs/ca-certificates.crt`).
const ANCHOR_PATHS: &[&str] = &[
    "usr/local/share/ca-certificates/airlock.crt", // Debian/Ubuntu/Alpine: update-ca-certificates
    "etc/pki/ca-trust/source/anchors/airlock.crt", // RHEL/Fedora/CentOS: update-ca-trust
    "etc/pki/trust/anchors/airlock.crt",           // openSUSE/SLES: update-ca-certificates
    "etc/ca-certificates/trust-source/anchors/airlock.crt", // Arch: trust extract-compat
];

/// tmpfs lowerdir with the merged CA bundles. It is above the image layers
/// in the overlayfs stack. Thus the container sees the project CA, and no
/// CA write goes to the persistent upperdir.
const OVERLAY_DIR: &str = "/mnt/ca-overlay";

/// Make a tmpfs lowerdir with a copy of each CA bundle of the image, with
/// the project CA added to each copy. Also writes the project CA to each
/// anchor path in [`ANCHOR_PATHS`].
///
/// Must run **before** overlayfs is mounted.
/// Args:
///  - `mounts`: Mount configuration with the project CA and image layers
///
/// Returns:
///   The tmpfs path, which the caller adds to `lowerdir`. `None` if there is
///   no project CA.
pub(super) fn prepare_overlay(mounts: &MountConfig) -> anyhow::Result<Option<&'static str>> {
    // For each bundle path, use the topmost layer that has a non-empty copy
    // of the file. Merge with this original layer content, not with the
    // merged overlayfs view. Otherwise the CA would be added again at each
    // reboot when the upperdir persists on the project disk.
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

    // Write the raw CA into each anchor directory, so trust-update tools
    // make bundles that include it. If a tool is not installed, the file is
    // not used and causes no problem.
    for rel in ANCHOR_PATHS {
        write_bundle(rel, &[], &mounts.ca_cert)?;
        debug!("ca: dropped anchor /{rel}");
    }
    Ok(Some(OVERLAY_DIR))
}

/// Find the topmost layer that has the file `rel` and return its contents.
/// Args:
///  - `layers`: Layer digests, topmost first
///  - `rel`: File path relative to the rootfs
///
/// Returns:
///   File contents, or `None` if no layer has a usable file at `rel`.
///
/// An empty file (or other non-file entry) means "hidden here". It can be
/// an overlayfs whiteout placeholder from the layer extractor, or an empty
/// bundle on purpose. The search stops there, so content that the image
/// hides does not come back.
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

/// Write `base` with `ca_cert` added to the end, at `rel` in the CA tmpfs.
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
