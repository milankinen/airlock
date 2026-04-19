//! Docker-daemon image export: check if an image exists locally and
//! stream-split its `docker image save` output into per-layer tarballs
//! staged under the shared layer cache.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::Read;
use std::path::PathBuf;
use std::process::Command;

use super::OciConfig;
use crate::cache;

/// Docker save manifest.json entry (Docker-specific, not OCI standard)
#[derive(serde::Deserialize)]
struct DockerManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// Output of [`save_layer_tarballs`]: parsed image config plus the ordered
/// layer digests (bottom-up, as `docker save` reports them).
pub struct DockerSave {
    /// Parsed image config (entrypoint, cmd, env, user).
    pub image_config: OciConfig,
    /// Layer digests in manifest order (bottom-up), with the `sha256:` prefix.
    pub layer_digests: Vec<String>,
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

/// Returns the architecture of a locally available Docker image (e.g. "amd64", "arm64").
pub fn image_arch(image_id: &str) -> Option<String> {
    let output = Command::new("docker")
        .args([
            "image",
            "inspect",
            "--format",
            "{{.Architecture}}",
            image_id,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let arch = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if arch.is_empty() { None } else { Some(arch) }
}

/// Stream `docker image save` and split its blobs into per-layer tarballs
/// staged under `~/.cache/airlock/oci/layers/`.
///
/// Every `blobs/sha256/<hex>` entry goes to `<hex>.download.tmp` unless
/// its hex already exists as a cached layer dir, in which case the bytes
/// are drained to `sink()` — avoids writing potentially gigabytes of
/// already-extracted base layers to disk just to delete them after
/// parsing the manifest. Once `manifest.json` has been parsed we know
/// which blob is the config and which are layers:
///
/// - Config blob → read into memory, returned in [`DockerSave`], and the
///   staging file is deleted.
/// - Layer blob (cached inline, skipped during stream) → no staging file
///   to clean up.
/// - Layer blob (not cached) → renamed to `<hex>.download`, ready for
///   `layer::ensure_layer_cached` to extract.
///
/// Any blob that's neither the config nor a manifest-listed layer is
/// dropped as unused. On any error, all staging files created by this
/// call are cleaned up before returning.
pub fn save_layer_tarballs(image_ref: &str) -> anyhow::Result<DockerSave> {
    let layers_root = cache::layers_root()?;

    let child = Command::new("docker")
        .args(["image", "save", image_ref])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let stdout = child.stdout.expect("piped stdout");
    let mut archive = tar::Archive::new(stdout);

    let mut manifest_json: Option<Vec<DockerManifestEntry>> = None;
    // hex → .download.tmp path, so we can rename or delete after parsing
    // the manifest. A HashMap because docker save may emit the same blob
    // multiple times across image tags.
    let mut staged: HashMap<String, PathBuf> = HashMap::new();

    let result = (|| -> anyhow::Result<DockerSave> {
        for entry in archive.entries()? {
            let mut entry = entry?;
            let path = entry.path()?.to_string_lossy().to_string();

            if path == "manifest.json" {
                let mut buf = Vec::new();
                entry.read_to_end(&mut buf)?;
                manifest_json = Some(serde_json::from_slice(&buf)?);
                continue;
            }
            let Some(hex) = path.strip_prefix("blobs/sha256/") else {
                continue;
            };
            if !entry.header().entry_type().is_file() {
                continue;
            }
            if staged.contains_key(hex) {
                // Same blob emitted twice — drain and ignore the duplicate.
                std::io::copy(&mut entry, &mut std::io::sink())?;
                continue;
            }
            // If this hex is already a cached layer, skip it inline. We
            // don't know yet whether it's classified as "layer" or "config"
            // in the manifest, but config blobs never collide with layer
            // digests (different content, different sha256), so a layer
            // dir hit can only be a cached layer.
            let digest = format!("sha256:{hex}");
            if cache::layer_dir(&digest).is_ok_and(|d| d.is_dir()) {
                std::io::copy(&mut entry, &mut std::io::sink())?;
                continue;
            }
            let tmp = layers_root.join(format!("{hex}.download.tmp"));
            let mut file = File::create(&tmp)?;
            std::io::copy(&mut entry, &mut file)?;
            staged.insert(hex.to_string(), tmp);
        }

        let manifest = manifest_json
            .and_then(|m| m.into_iter().next())
            .ok_or_else(|| anyhow::anyhow!("no manifest.json in docker save output"))?;

        let config_hex = manifest
            .config
            .strip_prefix("blobs/sha256/")
            .unwrap_or(&manifest.config)
            .to_string();
        let config_tmp = staged
            .remove(&config_hex)
            .ok_or_else(|| anyhow::anyhow!("config blob {config_hex} missing in docker save"))?;
        let image_config: OciConfig = serde_json::from_slice(&std::fs::read(&config_tmp)?)?;
        let _ = std::fs::remove_file(&config_tmp);

        // Rename staged layer blobs into `.download` for extraction.
        // Cached layers were dropped inline during streaming, so any layer
        // whose hex is still in `staged` is known to be non-cached.
        let mut layer_digests = Vec::with_capacity(manifest.layers.len());
        let mut seen: HashSet<String> = HashSet::new();
        for layer_ref in &manifest.layers {
            let hex = layer_ref
                .strip_prefix("blobs/sha256/")
                .unwrap_or(layer_ref)
                .to_string();
            let digest = format!("sha256:{hex}");
            layer_digests.push(digest.clone());
            if !seen.insert(hex.clone()) {
                continue;
            }
            let Some(tmp) = staged.remove(&hex) else {
                // Either cached (skipped in the stream) or a duplicate
                // already renamed above — nothing to do.
                continue;
            };
            let download = layers_root.join(format!("{hex}.download"));
            std::fs::rename(&tmp, &download)?;
        }

        Ok(DockerSave {
            image_config,
            layer_digests,
        })
    })();

    // Clean up any staging files still on disk (error paths, unused blobs).
    for (_, tmp) in staged {
        let _ = std::fs::remove_file(&tmp);
    }

    result
}
