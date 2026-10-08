//! OCI registry client.
//!
//! Finds the Linux image for the host CPU architecture in a remote registry
//! and downloads its layers. Each downloaded layer is verified against the image
//! manifest.

use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use indicatif::ProgressBar;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciImageManifest;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncWrite, AsyncWriteExt};

use super::OciConfig;

/// A resolved registry image with manifest and config.
pub struct RegistryImage {
    /// Parsed image reference.
    pub reference: Reference,
    /// Digest of the selected platform-specific manifest.
    pub digest: String,
    /// Digest of the multi-platform index that contains the manifest, if
    /// the reference resolved to an index. A user who pins `@sha256:…`
    /// usually copied the index digest (a registry shows that digest for a
    /// tag). Thus digest-pin checks must accept both digests.
    pub list_digest: Option<String>,
    /// Manifest of the selected platform.
    pub manifest: OciImageManifest,
    /// Parsed image config.
    pub image_config: OciConfig,
}

/// Create an OCI registry client. Uses plain HTTP if `insecure` is true,
/// and HTTPS if it is false.
fn make_client(insecure: bool) -> Client {
    let protocol = if insecure {
        ClientProtocol::Http
    } else {
        ClientProtocol::Https
    };
    Client::new(ClientConfig {
        protocol,
        platform_resolver: Some(Box::new(linux_platform_resolver)),
        ..Default::default()
    })
}

/// Select the `linux/<host-arch>` manifest from a multi-platform image index.
fn linux_platform_resolver(manifests: &[oci_client::manifest::ImageIndexEntry]) -> Option<String> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    manifests.iter().find_map(|m| {
        let p = m.platform.as_ref()?;
        if format!("{}", p.os) == "linux" && format!("{}", p.architecture) == arch {
            Some(m.digest.clone())
        } else {
            None
        }
    })
}

/// Return `true` if `e` is an OCI registry authentication failure.
pub fn is_auth_error(e: &anyhow::Error) -> bool {
    use oci_client::errors::OciDistributionError;
    e.downcast_ref::<OciDistributionError>().is_some_and(|err| {
        matches!(
            err,
            OciDistributionError::AuthenticationFailure(_)
                | OciDistributionError::UnauthorizedError { .. }
        )
    })
}

/// Resolve an image reference to a manifest, digest and config.
/// Args:
///  - `image_ref`: Image reference (e.g. `alpine:3.20`)
///  - `auth`: Registry auth
///  - `insecure`: Use plain HTTP instead of HTTPS.
///
/// Returns:
///   The resolved image, or error (for example an auth error, see
///   [`is_auth_error`]).
pub async fn resolve(
    image_ref: &str,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<RegistryImage> {
    let reference: Reference = image_ref.parse()?;
    let client = make_client(insecure);

    let (manifest, digest, config_str, list_digest) = client
        .pull_manifest_and_config_and_list_digest(&reference, auth)
        .await?;

    let image_config: OciConfig = serde_json::from_str(&config_str)?;

    Ok(RegistryImage {
        reference,
        digest,
        list_digest,
        manifest,
        image_config,
    })
}

/// Download a single layer blob to a file and verify its size and digest.
///
/// The caller must give a staging path for `dest`
/// (`<key>.download.tmp`), and rename it atomically after the return.
/// Args:
///  - `reference`: Image reference in the registry
///  - `layer`: Descriptor of the layer from the manifest
///  - `dest`: File to write the blob to
///  - `per_layer`: Optional progress bar of the layer
///  - `overall`: Optional progress bar of all layers
///  - `auth`: Registry auth
///  - `insecure`: Use plain HTTP instead of HTTPS.
///
/// Returns:
///   `Ok` if the blob has the size and digest from the manifest. Otherwise
///   error. A failed download or a size or digest mismatch removes `dest`.
pub async fn pull_layer(
    reference: &Reference,
    layer: &oci_client::manifest::OciDescriptor,
    dest: &Path,
    per_layer: Option<&ProgressBar>,
    overall: Option<&ProgressBar>,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<()> {
    let client = make_client(insecure);

    let registry = reference.resolve_registry();
    client.store_auth_if_needed(registry, auth).await;

    let file = tokio::fs::File::create(dest).await?;
    // Both progress bars get the same number of bytes as the writes.
    let bars: Vec<ProgressBar> = per_layer.into_iter().chain(overall).cloned().collect();
    let mut writer = HashingWriter {
        inner: ProgressWriter { inner: file, bars },
        hasher: Sha256::new(),
    };
    let pull_result = client.pull_blob(reference, layer, &mut writer).await;
    let flush_result = writer.flush().await;
    let digest = writer.hasher.finalize();

    if let Err(e) = pull_result {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(e.into());
    }
    flush_result?;

    // Check size and SHA-256 digest against the manifest. This protects
    // against a compromised or MITM registry that sends a different blob of
    // the same size. It does not depend on checks inside `oci-client`.
    let metadata = tokio::fs::metadata(dest).await?;
    let expected_size = layer.size as u64;
    if metadata.len() != expected_size {
        let _ = tokio::fs::remove_file(dest).await;
        anyhow::bail!(
            "layer size mismatch: expected {expected_size} bytes, got {}",
            metadata.len()
        );
    }

    let actual_digest = format!("sha256:{}", hex::encode(digest));
    if !actual_digest.eq_ignore_ascii_case(&layer.digest) {
        let _ = tokio::fs::remove_file(dest).await;
        anyhow::bail!(
            "layer digest mismatch: expected {}, got {actual_digest}",
            layer.digest
        );
    }
    Ok(())
}

/// File writer that increments all its progress bars on each write. Thus
/// the per-layer bar and the overall bar get the same byte stream.
struct ProgressWriter {
    inner: tokio::fs::File,
    bars: Vec<ProgressBar>,
}

impl AsyncWrite for ProgressWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                for bar in &this.bars {
                    bar.inc(n as u64);
                }
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// Writer that calculates the SHA-256 hash of the bytes it writes to the
/// inner writer. It hashes only the bytes that the inner writer accepted,
/// so after a partial write the hash still matches the content on disk.
struct HashingWriter {
    inner: ProgressWriter,
    hasher: Sha256,
}

impl AsyncWrite for HashingWriter {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = &mut *self;
        match Pin::new(&mut this.inner).poll_write(cx, buf) {
            Poll::Ready(Ok(n)) => {
                this.hasher.update(&buf[..n]);
                Poll::Ready(Ok(n))
            }
            other => other,
        }
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}
