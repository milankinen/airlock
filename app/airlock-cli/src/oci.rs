//! OCI image resolution, download, and extraction.
//!
//! Handles both Docker-daemon images and remote registry pulls, caches
//! layers and rootfs locally, and returns an `OciImage` with all the
//! metadata needed by the VM to start the container.

mod credentials;
mod docker;
mod layer;
mod registry;

use std::path::{Path, PathBuf};

use oci_client::config::ConfigFile as OciConfig;
use oci_client::secrets::RegistryAuth;

use crate::cli;
use crate::oci::credentials::ToRegistryAuth;
use crate::project::Project;

/// Metadata written to `image_dir/meta.json` after a successful image pull.
/// Hard-linked to `sandbox/image` — serves as both the GC ref (nlink > 1 means
/// in use) and the stored-digest source (replaces run.json image_digest field).
#[derive(serde::Serialize, serde::Deserialize)]
struct ImageMeta {
    digest: String,
    name: String,
}

/// Read `sandbox/image` as `ImageMeta`. Returns `None` if absent or not yet
/// in the new format (migration from old empty `.ref` hard-link).
fn read_sandbox_image_meta(sandbox_dir: &Path) -> Option<ImageMeta> {
    let data = std::fs::read(sandbox_dir.join("image")).ok()?;
    serde_json::from_slice(&data).ok()
}

/// Everything needed to configure the container process (returned by `prepare`).
/// Mount resolution, disk setup, and command/env overrides happen in `vm::start`.
pub struct OciImage {
    /// Path to the shared read-only image rootfs in the image cache.
    pub rootfs: PathBuf,
    /// OCI image digest, used by supervisor to detect image changes.
    pub image_id: String,
    /// Container home directory (e.g. `/root`), for guest-path `~` expansion.
    pub container_home: String,
    /// Container uid (from image config).
    pub uid: u32,
    /// Container gid (from image config).
    pub gid: u32,
    /// Raw image entrypoint+cmd merged, `/bin/sh` fallback if empty.
    /// No args.args overrides (those go in vm::start).
    pub cmd: Vec<String>,
    /// Base defaults (PATH/TERM/HOME) + image env.
    /// No sandbox.config.env overrides (those go in vm::start).
    pub env: Vec<String>,
}

/// Resolve, download, and prepare the OCI image for the sandbox.
pub async fn prepare(project: &Project) -> anyhow::Result<OciImage> {
    let stored_meta = read_sandbox_image_meta(&project.sandbox_dir);
    let image_cfg = &project.config.vm.image;
    let image_name = &image_cfg.name;

    // Fast path: if we already have a cached image for this name, skip
    // the network round-trip to resolve tag → digest.
    if let Some(ref meta) = stored_meta
        && meta.name == *image_name
    {
        let image_dir = crate::cache::image_dir(&meta.digest)?;
        if image_dir.join("rootfs").exists() && image_dir.join("meta.json").exists() {
            tracing::debug!("image cache hit for {image_name}");
            cli::log!(
                "  {} image cached {}",
                cli::check(),
                cli::dim(&meta.digest[..19.min(meta.digest.len())])
            );
            let overlay_dir = project.sandbox_dir.join("overlay");
            std::fs::create_dir_all(&overlay_dir)?;
            cli::log!("  {} environment ready", cli::check());
            return build_oci_image(&image_dir, meta.digest.clone());
        }
    }

    let stored_digest = stored_meta.map(|m| m.digest);

    // Set up registry auth: use stored credentials, fall back to anonymous.
    let registry_host: String = image_name
        .parse::<oci_client::Reference>()
        .map_or_else(|_| image_name.clone(), |r| r.resolve_registry().to_string());
    let mut auth = credentials::load(&project.vault, &registry_host)
        .map_or(RegistryAuth::Anonymous, |c| c.to_auth());

    // Resolve image reference to a digest. On auth failure, prompt for
    // credentials and retry in a loop until success or user interrupts.
    let mut image = loop {
        match resolve_image(image_name, image_cfg.resolution, image_cfg.insecure, &auth).await {
            Ok(img) => break img,
            Err(e) if registry::is_auth_error(&e) => {
                let creds = credentials::prompt(&registry_host)?;
                auth = creds.to_auth();
                match resolve_image(image_name, image_cfg.resolution, image_cfg.insecure, &auth)
                    .await
                {
                    Ok(img) => {
                        credentials::save(&project.vault, &registry_host, &creds)?;
                        break img;
                    }
                    Err(e) if registry::is_auth_error(&e) => {
                        cli::error!("authentication failed, try again");
                        // loop back to prompt again
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    };

    // Check if image changed before downloading
    let mut digest_changed = stored_digest
        .as_deref()
        .is_none_or(|s| s.trim() != image.digest);

    if let Some(old_digest) = stored_digest
        && digest_changed
    {
        match prompt_image_changed()? {
            ImageChangeAction::KeepOld => {
                // Only keep old if the cache is still intact; otherwise
                // fall through and let the new image be used.
                let old_image_dir = crate::cache::image_dir(old_digest.trim())?;
                if old_image_dir.join("rootfs").exists() && old_image_dir.join("meta.json").exists()
                {
                    digest_changed = false;
                    image.digest = old_digest.trim().to_string();
                }
            }
            ImageChangeAction::Recreate => {
                let spinner = cli::spinner("erasing old environment...");
                // Remove overlay files dir and CA overlay (both rebuilt on next start)
                let _ = std::fs::remove_dir_all(project.sandbox_dir.join("overlay"));
                let _ = std::fs::remove_dir_all(project.sandbox_dir.join("ca"));
                // Remove image ref hard link
                let _ = std::fs::remove_file(project.sandbox_dir.join("image"));
                spinner.finish_and_clear();
                cli::log!("  {} old environment erased", cli::check());
                // GC: remove image cache if no other sandbox references it
                gc_unused_image(old_digest.trim())?;
            }
            ImageChangeAction::Cancel => anyhow::bail!("cancelled by user"),
        }
    }

    // Download/ensure image (auth already resolved above).
    let image_dir = ensure_image(&mut image, image_name, &auth, image_cfg.insecure).await?;

    if digest_changed {
        // Hard-link meta.json into the sandbox directory.
        // Link count > 1 on meta.json means the image is still in use (GC guard).
        let meta_path = image_dir.join("meta.json");
        let sandbox_image = project.sandbox_dir.join("image");
        let _ = std::fs::remove_file(&sandbox_image);
        if let Err(e) = std::fs::hard_link(&meta_path, &sandbox_image) {
            tracing::debug!("image ref hard-link failed (cross-device?): {e}");
        }
    }

    let overlay_dir = project.sandbox_dir.join("overlay");
    std::fs::create_dir_all(&overlay_dir)?;
    cli::log!("  {} environment ready", cli::check());

    build_oci_image(&image_dir, image.digest)
}

/// Build an `OciImage` from a cached image directory.
fn build_oci_image(image_dir: &Path, digest: String) -> anyhow::Result<OciImage> {
    let config_path = image_dir.join("image_config.json");
    let image_config: OciConfig = if let Ok(data) = std::fs::read(&config_path) {
        serde_json::from_slice(&data).unwrap_or_default()
    } else {
        OciConfig::default()
    };

    let cfg = image_config.config.as_ref();
    let (uid, gid) = parse_user(cfg.and_then(|c| c.user.as_deref()).unwrap_or("0:0"));
    let rootfs = image_dir.join("rootfs");
    let container_home = lookup_home_dir(&rootfs, uid)?;

    // Resolve container command: entrypoint + cmd merged
    let cmd: Vec<String> = {
        let mut a = Vec::new();
        if let Some(ep) = cfg.and_then(|c| c.entrypoint.as_ref()) {
            a.extend(ep.iter().cloned());
        }
        if let Some(cmd) = cfg.and_then(|c| c.cmd.as_ref()) {
            a.extend(cmd.iter().cloned());
        }
        if a.is_empty() {
            a.push("/bin/sh".to_string());
        }
        a
    };

    // Resolve environment: base defaults → image env (no sandbox overrides here)
    let host_term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let mut env: Vec<String> = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        format!("TERM={host_term}"),
        format!("HOME={container_home}"),
    ];
    if let Some(image_env) = cfg.and_then(|c| c.env.as_ref()) {
        for e in image_env {
            let key = e.split('=').next().unwrap_or("");
            env.retain(|existing| !existing.starts_with(&format!("{key}=")));
            env.push(e.clone());
        }
    }

    Ok(OciImage {
        rootfs,
        image_id: digest,
        container_home,
        uid,
        gid,
        cmd,
        env,
    })
}

/// Wrap a command vector for execution inside a login shell.
///
/// Lone shell binaries (`sh`, `bash`, etc.) get `-l` appended directly.
/// All other commands are wrapped as `sh -l -c 'exec "$0" "$@"' cmd args...`
/// which passes arguments without quoting.
pub(crate) fn apply_login_shell(cmd: Vec<String>) -> Vec<String> {
    let is_lone_shell = cmd.len() == 1 && {
        let name = std::path::Path::new(&cmd[0])
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("");
        matches!(name, "sh" | "bash" | "zsh" | "fish" | "dash" | "ksh") || name.ends_with("sh")
    };
    if is_lone_shell {
        let mut result = cmd;
        result.push("-l".to_string());
        result
    } else {
        let mut result = vec![
            "bash".to_string(),
            "-l".to_string(),
            "-c".to_string(),
            r#"exec "$0" "$@""#.to_string(),
        ];
        result.extend(cmd);
        result
    }
}

/// Parse a `USER` string (`uid[:gid]`) into numeric uid/gid.
fn parse_user(user: &str) -> (u32, u32) {
    let parts: Vec<&str> = user.split(':').collect();
    let uid = parts.first().and_then(|s| s.parse().ok()).unwrap_or(0);
    let gid = parts.get(1).and_then(|s| s.parse().ok()).unwrap_or(0);
    (uid, gid)
}

enum ImageChangeAction {
    Recreate,
    KeepOld,
    Cancel,
}

fn prompt_image_changed() -> anyhow::Result<ImageChangeAction> {
    if !cli::is_interactive() {
        anyhow::bail!("sandbox image has changed");
    }
    let term = dialoguer::console::Term::stderr();
    let choice = dialoguer::Select::with_theme(&dialoguer::theme::ColorfulTheme::default())
        .with_prompt("Image has changed. What would you like to do?")
        .items([
            "Re-create environment",
            "Continue using old environment",
            "Cancel",
        ])
        .default(0)
        .clear(true)
        .interact_on_opt(&term)?
        .unwrap_or(2);
    let _ = term.clear_last_lines(1);

    Ok(match choice {
        0 => ImageChangeAction::Recreate,
        1 => ImageChangeAction::KeepOld,
        _ => ImageChangeAction::Cancel,
    })
}

/// Full image resolution (with config).
async fn resolve_image(
    image_ref: &str,
    resolution: crate::config::config::Resolution,
    insecure: bool,
    auth: &RegistryAuth,
) -> anyhow::Result<ResolvedImage> {
    use crate::config::config::Resolution;

    if !matches!(resolution, Resolution::Registry) {
        if let Some(image_id) = docker::image_exists(image_ref) {
            let host_arch = match std::env::consts::ARCH {
                "x86_64" => "amd64",
                "aarch64" => "arm64",
                other => other,
            };
            let docker_arch = docker::image_arch(&image_id).unwrap_or_default();
            if docker_arch.is_empty() || docker_arch == host_arch {
                cli::log!(
                    "  {} image resolved via docker {}",
                    cli::check(),
                    cli::dim(&image_id[..19.min(image_id.len())])
                );
                return Ok(ResolvedImage {
                    digest: image_id,
                    config: OciConfig::default(),
                    source: ImageSource::Docker {
                        image_ref: image_ref.to_string(),
                    },
                });
            }
            cli::log!(
                "  {} docker image is {docker_arch}, need {host_arch} — trying registry",
                cli::bullet()
            );
        }
        if matches!(resolution, Resolution::Docker) {
            anyhow::bail!("image {image_ref} not found in Docker daemon");
        }
    }

    let reg = registry::resolve(image_ref, auth, insecure).await?;
    cli::log!(
        "  {} image resolved {}",
        cli::check(),
        cli::dim(&format!("{}@{}", reg.reference, &reg.digest[..19]))
    );
    Ok(ResolvedImage {
        digest: reg.digest.clone(),
        config: reg.image_config.clone(),
        source: ImageSource::Registry(Box::new(reg)),
    })
}

/// An image resolved to a concrete digest, ready to be downloaded.
struct ResolvedImage {
    digest: String,
    config: OciConfig,
    source: ImageSource,
}

enum ImageSource {
    Docker { image_ref: String },
    Registry(Box<registry::RegistryImage>),
}

/// Ensure the image is fully downloaded and extracted in the cache.
/// Re-downloads if the extraction was incomplete.
async fn ensure_image(
    resolved: &mut ResolvedImage,
    image_name: &str,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<PathBuf> {
    let dir = crate::cache::image_dir(&resolved.digest)?;
    let rootfs = dir.join("rootfs");
    let meta_path = dir.join("meta.json");

    if rootfs.exists() && meta_path.exists() {
        if matches!(resolved.source, ImageSource::Docker { .. }) {
            let config_path = dir.join("image_config.json");
            if config_path.exists() {
                let data = std::fs::read(&config_path)?;
                resolved.config = serde_json::from_slice(&data)?;
            }
        }
        return Ok(dir);
    }

    // Migration: old cache has .complete but no meta.json — write meta.json and continue.
    let complete_marker = dir.join(".complete");
    if rootfs.exists() && complete_marker.exists() {
        let meta = ImageMeta {
            digest: resolved.digest.clone(),
            name: image_name.to_string(),
        };
        std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
        let _ = std::fs::remove_file(&complete_marker);
        if matches!(resolved.source, ImageSource::Docker { .. }) {
            let config_path = dir.join("image_config.json");
            if config_path.exists() {
                let data = std::fs::read(&config_path)?;
                resolved.config = serde_json::from_slice(&data).unwrap_or_default();
            }
        }
        return Ok(dir);
    }

    // Incomplete or corrupt image — clean up and re-extract
    if rootfs.exists() {
        tracing::debug!("image extraction incomplete, cleaning up");
        let _ = std::fs::remove_dir_all(&rootfs);
        let _ = std::fs::remove_file(&meta_path);
        let _ = std::fs::remove_file(&complete_marker);
    }

    std::fs::create_dir_all(&dir)?;

    // Clean up any .tmp files from interrupted downloads
    if let Ok(entries) = std::fs::read_dir(&dir) {
        for entry in entries.flatten() {
            if entry.path().extension().is_some_and(|ext| ext == "tmp") {
                let _ = std::fs::remove_file(entry.path());
            }
        }
    }

    match &resolved.source {
        ImageSource::Docker { image_ref } => {
            let sp = cli::spinner("exporting from docker...");
            resolved.config =
                docker::save_and_extract(image_ref, &rootfs, &dir.join("image_config.json"))?;
            let meta = ImageMeta {
                digest: resolved.digest.clone(),
                name: image_name.to_string(),
            };
            std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
            sp.finish_and_clear();
            cli::log!("  {} exported from docker", cli::check());
        }
        ImageSource::Registry(reg) => {
            let layers = &reg.manifest.layers;
            let total_bytes: u64 = layers.iter().map(|l| l.size as u64).sum();
            let pb = cli::progress_bar(total_bytes, "downloading");

            let mut layer_paths = Vec::new();
            for (i, layer_desc) in layers.iter().enumerate() {
                let layer_path = dir.join(format!("layer_{i}.tar.gz"));
                if registry::is_layer_valid(layer_desc, &layer_path) {
                    pb.inc(layer_desc.size as u64);
                } else {
                    let _ = std::fs::remove_file(&layer_path);
                    tokio::select! {
                        result = registry::pull_layer(&reg.reference, layer_desc, &layer_path, Some(&pb), auth, insecure) => {
                            result?;
                        }
                        () = cli::interrupted() => {
                            pb.finish_and_clear();
                            anyhow::bail!("interrupted");
                        }
                    }
                }
                layer_paths.push(layer_path);
            }
            pb.finish_and_clear();
            cli::log!(
                "  {} downloaded {}",
                cli::check(),
                cli::dim(&format!(
                    "{} layers, {}",
                    layers.len(),
                    format_size(total_bytes as i64)
                ))
            );

            let sp = cli::spinner("extracting layers...");
            let layer_refs: Vec<&Path> = layer_paths.iter().map(PathBuf::as_path).collect();
            layer::extract_layers(&layer_refs, &rootfs)?;
            sp.finish_and_clear();
            cli::log!("  {} extracted layers", cli::check());

            let config_json = serde_json::to_string_pretty(&resolved.config)?;
            std::fs::write(dir.join("image_config.json"), config_json)?;
            let meta = ImageMeta {
                digest: resolved.digest.clone(),
                name: image_name.to_string(),
            };
            std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;
        }
    }

    Ok(dir)
}

/// Remove an image from the cache if no sandbox holds a hard-link ref to it.
///
/// Each sandbox that uses an image hard-links `image_cache/meta.json` to
/// `sandbox/.airlock/sandbox/image`. A link count of 1 on `meta.json` means
/// only the cache's own copy remains — safe to delete.
fn gc_unused_image(digest: &str) -> anyhow::Result<()> {
    let image_dir = crate::cache::image_dir(digest)?;
    if !image_dir.exists() {
        return Ok(());
    }

    let meta_file = image_dir.join("meta.json");
    if meta_file.exists() {
        use std::os::unix::fs::MetadataExt;
        if std::fs::metadata(&meta_file).is_ok_and(|m| m.nlink() > 1) {
            // At least one sandbox still holds a hard link — keep the image.
            return Ok(());
        }
    }

    // No sandbox references this image — delete it
    let sp = cli::spinner("cleaning unused image...");
    let _ = std::fs::remove_dir_all(&image_dir);
    sp.finish_and_clear();
    cli::log!("  {} cleaned unused image", cli::check());
    Ok(())
}

fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Look up a user's home directory from the container rootfs /etc/passwd.
fn lookup_home_dir(rootfs: &Path, uid: u32) -> anyhow::Result<String> {
    let passwd_path = rootfs.join("etc/passwd");
    let content = std::fs::read_to_string(&passwd_path)
        .map_err(|e| anyhow::anyhow!("cannot read container /etc/passwd: {e}"))?;

    for line in content.lines() {
        let fields: Vec<&str> = line.split(':').collect();
        if fields.len() >= 6 && fields[2].parse::<u32>().ok() == Some(uid) {
            return Ok(fields[5].to_string());
        }
    }

    anyhow::bail!("no home directory found for uid {uid} in container /etc/passwd")
}
