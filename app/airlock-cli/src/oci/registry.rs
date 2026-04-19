//! OCI registry client: resolve image manifests, pull layers, and verify
//! downloads.

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

/// A fully resolved registry image with manifest and config.
pub struct RegistryImage {
    pub reference: Reference,
    pub digest: String,
    pub manifest: OciImageManifest,
    pub image_config: OciConfig,
}

/// Create an OCI registry client. Uses plain HTTP when `insecure` is true,
/// HTTPS otherwise.
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

/// Returns true if `e` is an OCI registry authentication failure.
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

/// Resolve an image reference to a manifest, digest, and config.
pub async fn resolve(
    image_ref: &str,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<RegistryImage> {
    let reference: Reference = image_ref.parse()?;
    let client = make_client(insecure);

    let (manifest, digest, config_str) = client.pull_manifest_and_config(&reference, auth).await?;

    let image_config: OciConfig = serde_json::from_str(&config_str)?;

    Ok(RegistryImage {
        reference,
        digest,
        manifest,
        image_config,
    })
}

/// Download a single layer blob to `dest` with optional progress reporting.
///
/// The caller is responsible for placing `dest` at the appropriate staging
/// path (`<digest>.download.tmp`) and for atomically renaming into place
/// after return. Both the per-layer and overall progress bars, when
/// provided, are incremented by the same number of bytes as data is
/// written.
///
/// The blob is hashed with SHA-256 while streaming and compared against the
/// digest from the manifest before return; on mismatch the staged file is
/// removed and an error is surfaced. Size is also checked. This protects
/// against a compromised or MITM'd registry serving a same-size, different
/// blob, and does not depend on transitive checks inside `oci-client`.
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

/// Wraps an `AsyncWrite` and increments every attached progress bar on each
/// write. Used to drive the per-layer bar and the overall-total bar from the
/// same byte stream.
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

/// Feeds the byte stream through a SHA-256 hasher on the way to the inner
/// writer. Only the bytes actually accepted by the inner writer are hashed
/// so partial-write short counts stay consistent with on-disk content.
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
