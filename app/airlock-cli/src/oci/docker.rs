//! Docker-daemon image export: check if an image exists locally and
//! stream-split its `docker image save` output into per-layer tarballs
//! staged under the shared layer cache.

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use sha2::{Digest, Sha256};

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

/// Returns the registry digests the daemon recorded for a local image, i.e.
/// the `RepoDigests` entries with their `<repo>@` prefix stripped.
///
/// Used to check a digest-pinned reference against the daemon's copy. The
/// list is empty for images that were never pulled from (or pushed to) a
/// registry — a locally built image has no registry identity, so a pin can
/// never be satisfied from Docker alone.
pub fn repo_digests(image_id: &str) -> Vec<String> {
    let Ok(output) = Command::new("docker")
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

/// Drop guard that kills and reaps a `docker image save` child when the
/// enclosing future is cancelled. Successful callers `take()` the child
/// out first so the guard is a no-op on the happy path.
struct DockerSaveGuard(Option<Child>);

impl Drop for DockerSaveGuard {
    fn drop(&mut self) {
        if let Some(mut child) = self.0.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
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
///
/// The tar-streaming loop runs on `spawn_blocking`; the outer future owns
/// the docker child via a drop guard, so a cancelled future (e.g. Ctrl+C
/// via a parent `tokio::select!`) kills docker, which closes stdout, which
/// lets the detached blocking task finish promptly.
pub async fn save_layer_tarballs(image_ref: &str) -> anyhow::Result<DockerSave> {
    let layers_root = cache::layers_root()?;

    let mut child = Command::new("docker")
        .args(["image", "save", image_ref])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let stdout = child.stdout.take().expect("piped stdout");
    let mut guard = DockerSaveGuard(Some(child));

    let result =
        tokio::task::spawn_blocking(move || save_from_stream(stdout, &layers_root)).await?;

    // Success path: reap the docker child normally so the guard's Drop
    // doesn't try to kill an already-exited process.
    if let Some(mut child) = guard.0.take() {
        let _ = child.wait();
    }
    result
}

/// Copy `reader` into `writer` while computing the SHA-256 of the bytes,
/// returning the lowercase hex digest. Lets the docker staging path verify
/// a blob's content against its claimed `blobs/sha256/<hex>` name in a
/// single streaming pass, the same guarantee `registry::pull_layer` gives
/// registry downloads.
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

/// Sync tar-streaming pipeline. Consumes `docker image save` stdout and
/// produces a [`DockerSave`] plus pre-staged `.download` files for every
/// non-cached layer. Separated from the async wrapper so the whole
/// blocking I/O loop is a single `spawn_blocking` unit.
fn save_from_stream<R: Read>(stdout: R, layers_root: &Path) -> anyhow::Result<DockerSave> {
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
            if cache::layer_dir(&cache::layer_key(&digest)).is_ok_and(|d| d.is_dir()) {
                std::io::copy(&mut entry, &mut std::io::sink())?;
                continue;
            }
            let tmp = layers_root.join(format!("{}.download.tmp", cache::layer_key(&digest)));
            let mut file = File::create(&tmp)?;
            let actual = copy_hashing(&mut entry, &mut file)?;
            // Cross-source cache poisoning guard: docker hands us the digest
            // only as the `blobs/sha256/<hex>` member name, never as verified
            // content. Hash the bytes we just wrote and reject the blob before
            // it is renamed into the shared cache, where a later registry pull
            // for the same digest would otherwise trust it. Mirrors the
            // SHA-256 check on the registry path (see `registry::pull_layer`).
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
            let download = layers_root.join(format!("{}.download", cache::layer_key(&digest)));
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

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;
    use crate::cache::HOME_LOCK;

    /// Build a plain tar from in-memory `(path, content)` entries — mirrors
    /// what `docker image save` emits with the classic driver.
    fn build_tar(entries: &[(&str, &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        {
            let mut b = tar::Builder::new(&mut buf);
            for (path, content) in entries {
                let mut header = tar::Header::new_gnu();
                header.set_size(content.len() as u64);
                header.set_mode(0o644);
                header.set_cksum();
                b.append_data(&mut header, path, *content).unwrap();
            }
            b.finish().unwrap();
        }
        buf
    }

    fn temp_home() -> PathBuf {
        let base = std::env::temp_dir().join(format!(
            "airlock-docker-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    #[test]
    fn copy_hashing_matches_known_vectors() {
        let mut out = Vec::new();
        assert_eq!(
            copy_hashing(Cursor::new(b"abc".to_vec()), &mut out).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(out, b"abc");

        let mut empty = Vec::new();
        assert_eq!(
            copy_hashing(Cursor::new(Vec::new()), &mut empty).unwrap(),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn save_from_stream_rejects_blob_with_mismatched_digest() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let home = temp_home();
        unsafe {
            std::env::set_var("HOME", &home);
        }
        let layers_root = cache::layers_root().unwrap();

        // A poisoned blob: named sha256:aaaa... but its content hashes to
        // something else entirely. This is the cross-source poisoning attempt.
        let claimed = "a".repeat(64);
        let tar = build_tar(&[(&format!("blobs/sha256/{claimed}"), b"poison")]);

        let Err(err) = save_from_stream(Cursor::new(tar), &layers_root) else {
            panic!("mismatched docker blob must be rejected");
        };
        assert!(
            err.to_string().contains("digest mismatch"),
            "unexpected error: {err}"
        );

        // The staged tmp file must not be left behind.
        let tmp = layers_root.join(format!(
            "{}.download.tmp",
            cache::layer_key(&format!("sha256:{claimed}"))
        ));
        assert!(!tmp.exists(), "staged tmp file leaked after rejection");
    }
}
