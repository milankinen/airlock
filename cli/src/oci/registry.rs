use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use indicatif::ProgressBar;
use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciImageManifest;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use tokio::io::{AsyncWrite, AsyncWriteExt};

pub struct RegistryImage {
    pub reference: Reference,
    pub digest: String,
    pub manifest: OciImageManifest,
    pub image_config: oci_client::config::ConfigFile,
}

fn make_client() -> Client {
    Client::new(ClientConfig {
        protocol: ClientProtocol::Https,
        platform_resolver: Some(Box::new(linux_arm64_resolver)),
        ..Default::default()
    })
}

fn linux_arm64_resolver(manifests: &[oci_client::manifest::ImageIndexEntry]) -> Option<String> {
    manifests.iter().find_map(|m| {
        let p = m.platform.as_ref()?;
        if format!("{}", p.os) == "linux" && format!("{}", p.architecture) == "arm64" {
            Some(m.digest.clone())
        } else {
            None
        }
    })
}

pub async fn resolve(image_ref: &str) -> anyhow::Result<RegistryImage> {
    let reference: Reference = image_ref.parse()?;
    let client = make_client();
    let auth = RegistryAuth::Anonymous;

    let (manifest, digest, config_str) = client.pull_manifest_and_config(&reference, &auth).await?;

    let image_config: oci_client::config::ConfigFile = serde_json::from_str(&config_str)?;

    Ok(RegistryImage {
        reference,
        digest,
        manifest,
        image_config,
    })
}

pub async fn pull_layer(
    reference: &Reference,
    layer: &oci_client::manifest::OciDescriptor,
    dest: &Path,
    progress: Option<&ProgressBar>,
) -> anyhow::Result<()> {
    let client = make_client();
    let auth = RegistryAuth::Anonymous;

    let registry = reference.resolve_registry();
    client.store_auth_if_needed(registry, &auth).await;

    // Download to a temp file, then rename on success
    let tmp = dest.with_extension("tmp");
    let file = tokio::fs::File::create(&tmp).await?;
    let mut writer: Box<dyn AsyncWrite + Unpin> = if let Some(pb) = progress {
        Box::new(ProgressWriter {
            inner: file,
            progress: pb.clone(),
        })
    } else {
        Box::new(file)
    };
    client.pull_blob(reference, layer, &mut writer).await?;
    writer.flush().await?;
    drop(writer);

    // Verify size
    let metadata = tokio::fs::metadata(&tmp).await?;
    let expected = layer.size as u64;
    if metadata.len() != expected {
        let _ = tokio::fs::remove_file(&tmp).await;
        anyhow::bail!(
            "layer size mismatch: expected {expected} bytes, got {}",
            metadata.len()
        );
    }

    // Atomically move into place
    tokio::fs::rename(&tmp, dest).await?;
    Ok(())
}

/// Check if a previously downloaded layer file is valid (correct size).
pub fn is_layer_valid(layer: &oci_client::manifest::OciDescriptor, path: &Path) -> bool {
    path.metadata().is_ok_and(|m| m.len() == layer.size as u64)
}

/// Wraps an `AsyncWrite` and increments a progress bar on each write.
struct ProgressWriter {
    inner: tokio::fs::File,
    progress: ProgressBar,
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
                this.progress.inc(n as u64);
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
