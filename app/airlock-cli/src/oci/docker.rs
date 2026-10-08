//! Local container engine support.
//!
//! Finds images in a local Docker or Podman engine, reads their metadata and
//! exports their layers into the shared layer cache. Docker and Podman accept
//! the same commands for all of these operations.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use sha2::{Digest, Sha256};

use super::{OciConfig, layer};
use crate::cache;

/// Get the digest hex of a blob member, or `None` for metadata. Docker 25+
/// writes `blobs/sha256/<hex>`. The podman `docker-archive` format writes
/// `<hex>.tar` and `<hex>.json`. The legacy `<id>/layer.tar` is not
/// content-addressed.
fn blob_hex(path: &str) -> Option<&str> {
    let hex = path
        .strip_prefix("blobs/sha256/")
        .or_else(|| path.strip_suffix(".tar"))
        .or_else(|| path.strip_suffix(".json"))?;
    (hex.len() == 64 && hex.bytes().all(|b| b.is_ascii_hexdigit())).then_some(hex)
}

/// Entry of the `manifest.json` of `docker save` (Docker-specific, not OCI
/// standard).
#[derive(serde::Deserialize)]
struct DockerManifestEntry {
    #[serde(rename = "Config")]
    config: String,
    #[serde(rename = "Layers")]
    layers: Vec<String>,
}

/// Output of [`save_layer_tarballs`]: the parsed image config and the layer
/// digests.
pub struct DockerSave {
    /// Parsed image config (entrypoint, cmd, env, user).
    pub image_config: OciConfig,
    /// Layer digests in manifest order (bottom first), with the `sha256:`
    /// prefix.
    pub layer_digests: Vec<String>,
}

/// Check if an image exists in the local engine.
/// Args:
///  - `engine`: `docker` or `podman`
///  - `image_ref`: Image reference (repo:tag, without a digest).
///
/// Returns:
///   The image ID, or `None` if the image is not found.
pub fn image_exists(engine: &str, image_ref: &str) -> Option<String> {
    // Use `docker images`, not `docker image inspect`. Docker Desktop with
    // containerd snapshotting can list images, but the inspect by tag can
    // fail.
    let output = Command::new(engine)
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

    // Use only the first line (if there are multiple matches).
    Some(id.lines().next().unwrap_or(&id).to_string())
}

/// Return the architecture of a local image (e.g. `amd64`, `arm64`).
pub fn image_arch(engine: &str, image_id: &str) -> Option<String> {
    let output = Command::new(engine)
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

/// Return the `USER` from the config of a local image (`""` if none).
/// Fails if the engine cannot give it, because the caller must not guess.
pub fn image_user(engine: &str, image_id: &str) -> anyhow::Result<String> {
    let output = Command::new(engine)
        .args(["image", "inspect", "--format", "{{.Config.User}}", image_id])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| anyhow::anyhow!("failed to run {engine} image inspect: {e}"))?;
    if !output.status.success() {
        anyhow::bail!(
            "{engine} image inspect {image_id} failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
}

/// Return the registry digests that the engine recorded for a local image.
/// These are the `RepoDigests` entries without their `<repo>@` prefix.
///
/// Used to check a digest-pinned reference against the local copy. The list
/// is empty for an image that was never pulled from (or pushed to) a
/// registry. A locally built image has no registry identity, so Docker alone
/// can never satisfy a pin for it.
pub fn repo_digests(engine: &str, image_id: &str) -> Vec<String> {
    let Ok(output) = Command::new(engine)
        .args([
            "image",
            "inspect",
            "--format",
            "{{range .RepoDigests}}{{println .}}{{end}}",
            image_id,
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output()
    else {
        return Vec::new();
    };
    if !output.status.success() {
        return Vec::new();
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().rsplit_once('@'))
        .map(|(_, digest)| digest.to_string())
        .collect()
}

/// Drop guard that kills and reaps a `docker image save` child if the
/// future that owns it is cancelled. On success, the caller does `take()`
/// on the child first, so the guard then does nothing.
struct DockerSaveGuard(Option<Child>);

impl Drop for DockerSaveGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

/// Export a local image and stage its layers as tarballs under
/// `~/.cache/airlock/oci/layers/`.
///
/// Each layer that is not already cached becomes a `<key>.download` file,
/// ready for [`super::layer::ensure_layer_cached`] to extract. If the
/// future is cancelled (e.g. Ctrl+C), the engine process stops.
/// Args:
///  - `engine`: `docker` or `podman`
///  - `image_ref`: Image reference to export.
///
/// Returns:
///   The parsed image config and the layer digests. On error, all staging
///   files of this call are removed.
pub async fn save_layer_tarballs(engine: &str, image_ref: &str) -> anyhow::Result<DockerSave> {
    let layers_root = cache::layers_root()?;

    let mut child = Command::new(engine)
        .args(["image", "save", image_ref])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut guard = DockerSaveGuard(Some(child));

    // The tar loop runs in `spawn_blocking`. This future owns the docker
    // child through the drop guard. If the future is cancelled (e.g. Ctrl+C
    // in a parent `tokio::select!`), the guard kills docker. That closes
    // stdout, so the detached blocking task stops quickly.
    let result =
        tokio::task::spawn_blocking(move || save_from_stream(stdout, &layers_root)).await?;

    // Success: reap the docker child normally, so the `Drop` of the guard
    // does not try to kill a process that already exited.
    if let Some(mut child) = guard.0.take() {
        let _ = child.wait();
    }
    result
}

/// Copy `reader` into `writer` and calculate the SHA-256 of the bytes.
/// Returns:
///   The lowercase hex digest. The caller uses it to check the blob content
///   against its `blobs/sha256/<hex>` name in one pass, the same check that
///   [`super::registry::pull_layer`] does for registry downloads.
fn copy_hashing<R: Read, W: Write>(mut reader: R, mut writer: W) -> std::io::Result<String> {
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        writer.write_all(&buf[..n])?;
    }
    Ok(hex::encode(hasher.finalize()))
}

/// Read the `docker image save` output and stage the layers. This is a
/// separate sync function, so that all blocking I/O runs in one
/// `spawn_blocking` task.
///
/// Each blob goes to a process-unique `<key>.download.<pid>.<seq>.tmp`, but a blob that is already a
/// cached layer dir goes to `sink()`. Thus airlock does not write possibly
/// gigabytes of extracted base layers to disk only to delete them later.
/// After the parse of `manifest.json`, the function knows which blob is the
/// config and which blobs are layers:
///  * Config blob: read into memory, returned in [`DockerSave`], and the
///    staging file is deleted.
///  * Cached layer blob: skipped in the stream, so there is no staging file.
///  * Layer blob that is not cached: renamed to `<key>.download`.
///
/// A blob that is not the config and not a layer in the manifest is
/// deleted as unused.
/// Args:
///  - `stdout`: Output stream of `docker image save`
///  - `layers_root`: Root directory of the layer cache.
///
/// Returns:
///   The parsed image config and the layer digests, bottom first.
pub(super) fn save_from_stream<R: Read>(
    stdout: R,
    layers_root: &Path,
) -> anyhow::Result<DockerSave> {
    let mut archive = tar::Archive::new(stdout);

    let mut manifest_json: Option<Vec<DockerManifestEntry>> = None;
    // Map from hex to staging path, to rename or delete the files
    // after the manifest parse. A HashMap, because docker save may write the
    // same blob multiple times for different image tags.
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
            let Some(hex) = blob_hex(&path) else {
                continue;
            };
            if !entry.header().entry_type().is_file() {
                continue;
            }
            if staged.contains_key(hex) {
                // The same blob again. Read and ignore the duplicate.
                std::io::copy(&mut entry, &mut std::io::sink())?;
                continue;
            }
            // Skip this blob if it is already a cached layer. The manifest is
            // not parsed yet, so the blob can be a layer or the config. A
            // config blob never has the same digest as a layer (different
            // content), so a layer dir match is always a cached layer.
            let digest = format!("sha256:{hex}");
            if cache::layer_dir(&cache::layer_key(&digest)).is_ok_and(|d| d.is_dir()) {
                std::io::copy(&mut entry, &mut std::io::sink())?;
                continue;
            }
            let tmp = layer::download_tmp_path(layers_root, &cache::layer_key(&digest));
            let mut file = File::create(&tmp)?;
            let actual = copy_hashing(&mut entry, &mut file)?;
            // Prevent cross-source cache poisoning. The member name gives the
            // digest, but Docker does not verify the content. Reject a blob
            // with a wrong hash before it goes into the shared cache, because
            // a later registry pull of the same digest trusts the cache.
            // `registry::pull_layer` does the same check.
            if !actual.eq_ignore_ascii_case(hex) {
                drop(file);
                let _ = std::fs::remove_file(&tmp);
                anyhow::bail!(
                    "docker layer digest mismatch: blob named sha256:{hex} \
                     hashes to sha256:{actual}"
                );
            }
            staged.insert(hex.to_string(), tmp);
        }

        let manifest = manifest_json
            .and_then(|m| m.into_iter().next())
            .ok_or_else(|| anyhow::anyhow!("no manifest.json in docker save output"))?;

        let config_hex = blob_hex(&manifest.config)
            .unwrap_or(&manifest.config)
            .to_string();
        let config_tmp = staged
            .remove(&config_hex)
            .ok_or_else(|| anyhow::anyhow!("config blob {config_hex} missing in docker save"))?;
        let image_config: OciConfig = serde_json::from_slice(&std::fs::read(&config_tmp)?)?;
        let _ = std::fs::remove_file(&config_tmp);

        // Rename the staged layer blobs to `.download` for extraction. The
        // stream loop skipped cached layers, so each layer that is still in
        // `staged` is not cached.
        let mut layer_digests = Vec::with_capacity(manifest.layers.len());
        let mut seen: HashSet<String> = HashSet::new();
        for layer_ref in &manifest.layers {
            let hex = blob_hex(layer_ref).unwrap_or(layer_ref).to_string();
            let digest = format!("sha256:{hex}");
            layer_digests.push(digest.clone());
            if !seen.insert(hex.clone()) {
                continue;
            }
            let Some(tmp) = staged.remove(&hex) else {
                // Cached (skipped in the stream) or a duplicate that was
                // already renamed. Nothing to do.
                continue;
            };
            let download = layers_root.join(format!("{}.download", cache::layer_key(&digest)));
            std::fs::rename(&tmp, &download)?;
        }

        Ok(DockerSave {
            image_config,
            layer_digests,
        })
    })();

    // Remove the staging files that are still on disk (errors, unused blobs).
    for (_, tmp) in staged {
        let _ = std::fs::remove_file(&tmp);
    }

    result
}
