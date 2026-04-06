use std::fs::File;
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
/// Streams `docker image save` output, writing blobs to temporary files
/// to avoid loading entire layers into memory. Then unpacks layers from
/// the files in order.
pub fn save_and_extract(
    image_ref: &str,
    rootfs: &Path,
    image_config_dest: &Path,
) -> anyhow::Result<OciConfig> {
    std::fs::create_dir_all(rootfs)?;

    let blob_dir = image_config_dest
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid image config path"))?;

    let child = Command::new("docker")
        .args(["image", "save", image_ref])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.expect("piped stdout");
    let mut archive = tar::Archive::new(stdout);

    let mut manifest_json: Option<Vec<DockerManifestEntry>> = None;

    // Stream through the tar, writing blobs to disk
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = entry.path()?.to_string_lossy().to_string();

        if path == "manifest.json" {
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf)?;
            manifest_json = Some(serde_json::from_slice(&buf)?);
        } else if path.starts_with("blobs/") && entry.header().entry_type().is_file() {
            let dest = blob_dir.join(&path);
            if let Some(parent) = dest.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let mut file = File::create(&dest)?;
            std::io::copy(&mut entry, &mut file)?;
        }
    }

    let manifest = manifest_json
        .and_then(|m| m.into_iter().next())
        .ok_or_else(|| anyhow::anyhow!("no manifest.json in docker save output"))?;

    // Parse the config blob
    let config_path = blob_dir.join(&manifest.config);
    let config_bytes = std::fs::read(&config_path)?;
    let image_config: OciConfig = serde_json::from_slice(&config_bytes)?;
    std::fs::write(image_config_dest, &config_bytes)?;

    // Extract layers in order from disk files
    for layer_ref in &manifest.layers {
        let layer_path = blob_dir.join(layer_ref);
        let file = File::open(&layer_path)?;

        // Layer blobs may be gzip-compressed or plain tar — peek at magic bytes
        let mut buf_reader = std::io::BufReader::new(file);
        let mut magic = [0u8; 2];
        let n = buf_reader.read(&mut magic)?;

        let reader: Box<dyn Read> = if n == 2 && magic == [0x1f, 0x8b] {
            Box::new(flate2::read::GzDecoder::new(
                std::io::Cursor::new(magic).chain(buf_reader),
            ))
        } else {
            Box::new(std::io::Cursor::new(magic[..n].to_vec()).chain(buf_reader))
        };

        let mut layer_archive = tar::Archive::new(reader);
        layer_archive.set_preserve_permissions(true);
        layer_archive.set_overwrite(true);
        layer_archive.unpack(rootfs)?;
    }

    // Clean up blob files (rootfs is already extracted)
    let _ = std::fs::remove_dir_all(blob_dir.join("blobs"));

    Ok(image_config)
}
