use std::path::Path;

use oci_client::client::{ClientConfig, ClientProtocol};
use oci_client::manifest::OciImageManifest;
use oci_client::secrets::RegistryAuth;
use oci_client::{Client, Reference};
use tokio::io::AsyncWriteExt;

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

    eprintln!("  resolved {}@{}", reference, &digest[..19]);

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
) -> anyhow::Result<()> {
    let client = make_client();
    let auth = RegistryAuth::Anonymous;

    // Store auth so pull_blob can use it
    let registry = reference.resolve_registry();
    client.store_auth_if_needed(&registry, &auth).await;

    let mut file = tokio::fs::File::create(dest).await?;
    client.pull_blob(reference, layer, &mut file).await?;
    file.flush().await?;
    Ok(())
}
