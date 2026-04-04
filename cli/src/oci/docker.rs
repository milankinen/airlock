use std::io::Read;
use std::path::Path;
use std::process::Command;

use super::OciConfig;

/// Docker save manifest.json entry (Docker-specific, not OCI standard)
#[derive(serde::Deserialize)]
struct DockerManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// Check if an image exists in the local Docker daemon.
/// Returns the image ID if found.
///
/// Uses `docker images` instead of `docker image inspect` because
/// Docker Desktop with containerd-snapshotting can list images but
/// fail to inspect by tag.
pub fn image_exists(image_ref: &str) -> Option<String> {
    let output = Command::new("docker")
        .args(["images", image_ref, "--format", "{{.ID}}", "--no-trunc"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }

    let id = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if id.is_empty() {
        return None;
    }

    // Take only the first line (in case of multiple matches)
    Some(id.lines().next().unwrap_or(&id).to_string())
}

/// Extract an image from the local Docker daemon into a rootfs directory,
/// and return the parsed image config.
///
/// Uses `docker image save` which outputs an OCI image layout tar.
/// No container creation needed.
pub fn save_and_extract(
    image_ref: &str,
    rootfs: &Path,
    image_config_dest: &Path,
) -> anyhow::Result<OciConfig> {
    std::fs::create_dir_all(rootfs)?;

    let child = Command::new("docker")
        .args(["image", "save", image_ref])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.expect("piped stdout");
    let mut archive = tar::Archive::new(stdout);

    let mut manifest_json: Option<Vec<DockerManifestEntry>> = None;
    let mut config_data: Option<Vec<u8>> = None;
    let mut layer_data: Vec<(String, Vec<u8>)> = Vec::new();

    // Stream through the tar, collecting manifest, config, and layers
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();

        if path == "manifest.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            manifest_json = Some(serde_json::from_slice(&buf)?);
        } else if path.starts_with("blobs/") {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            layer_data.push((path, buf));
        }
    }

    let manifest = manifest_json
        .and_then(|m| m.into_iter().next())
        .ok_or_else(|| anyhow::anyhow!("no manifest.json in docker save output"))?;

    // Find and parse the config blob
    for (path, data) in &layer_data {
        if *path == manifest.config {
            config_data = Some(data.clone());
            break;
        }
    }
    let config_bytes = config_data
        .ok_or_else(|| anyhow::anyhow!("config blob not found in docker save output"))?;
    let image_config: OciConfig = serde_json::from_slice(&config_bytes)?;

    // Save config for caching
    std::fs::write(image_config_dest, &config_bytes)?;

    // Extract layers in order
    for layer_path in &manifest.layers {
        let data = layer_data
            .iter()
            .find(|(p, _)| p == layer_path)
            .map(|(_, d)| d)
            .ok_or_else(|| anyhow::anyhow!("layer blob not found: {layer_path}"))?;

        let mut layer_archive = tar::Archive::new(data.as_slice());
        // Set options to handle permissions/ownership gracefully
        layer_archive.set_preserve_permissions(true);
        layer_archive.set_overwrite(true);
        layer_archive.unpack(rootfs)?;
    }

    Ok(image_config)
}
