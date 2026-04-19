//! OCI image resolution, download, and extraction.
//!
//! Handles both Docker-daemon images and remote registry pulls, caches
//! layers and rootfs locally, and returns an `OciImage` with all the
//! metadata needed by the VM to start the container.

mod credentials;
mod docker;
mod gc;
mod layer;
mod registry;

use std::path::{Path, PathBuf};

use futures::stream::{self, StreamExt};
pub use gc::sweep as gc_sweep;
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
    /// Ordered layer digests — topmost-first. The guest passes these as
    /// overlayfs lowerdirs pointing under `/mnt/layers/<digest>/rootfs`.
    /// Missing on older cached images; callers must tolerate an empty vec.
    #[serde(default)]
    layers: Vec<String>,
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
    /// OCI image digest, used by supervisor to detect image changes.
    pub image_id: String,
    /// Ordered layer digests — topmost-first. The guest mounts the shared
    /// `/mnt/layers/<digest>/rootfs` cache as overlayfs lowerdirs in this order.
    pub image_layers: Vec<String>,
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
    // the network round-trip to resolve tag → digest. The image is
    // considered ready when `meta.json` exists and every listed layer
    // has its `.ok` marker — there's no merged rootfs to check anymore.
    if let Some(ref meta) = stored_meta
        && meta.name == *image_name
        && !meta.layers.is_empty()
    {
        let image_dir = crate::cache::image_dir(&meta.digest)?;
        if image_dir.join("meta.json").exists()
            && meta.layers.iter().all(|d| {
                crate::cache::layer_dir(d)
                    .map(|p| p.join(".ok").exists())
                    .unwrap_or(false)
            })
        {
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
                if old_image_dir.join("meta.json").exists() {
                    digest_changed = false;
                    image.digest = old_digest.trim().to_string();
                }
            }
            ImageChangeAction::Recreate => {
                let spinner = cli::spinner("erasing old environment...");
                // File-mount staging dir is rebuilt on every start.
                let _ = std::fs::remove_dir_all(project.sandbox_dir.join("overlay"));
                // Legacy CA overlay dir from before CA moved to RPC.
                let _ = std::fs::remove_dir_all(project.sandbox_dir.join("ca"));
                // Remove image ref hard link — drops this sandbox's liveness signal
                // for the old image, so the sweep below may collect it.
                let _ = std::fs::remove_file(project.sandbox_dir.join("image"));
                spinner.finish_and_clear();
                cli::log!("  {} old environment erased", cli::check());
                // GC: remove images with no remaining sandbox refs, plus any
                // layers they uniquely owned.
                gc::sweep();
            }
            ImageChangeAction::Cancel => anyhow::bail!("cancelled by user"),
        }
    }

    // Download/ensure image (auth already resolved above).
    let image_dir = ensure_image(&mut image, image_name, &auth, image_cfg.insecure).await?;

    if digest_changed {
        // Hard-link meta.json into the sandbox directory. Link count > 1 on
        // meta.json is the GC guard — without it, a sibling sandbox creating
        // a new image could trigger a sweep that wrongly deletes this one.
        // Fail hard on error: the cache and the sandbox should live on the
        // same filesystem under $HOME, so the only realistic cause is a
        // config problem we want to surface.
        let meta_path = image_dir.join("meta.json");
        let sandbox_image = project.sandbox_dir.join("image");
        let _ = std::fs::remove_file(&sandbox_image);
        std::fs::hard_link(&meta_path, &sandbox_image).map_err(|e| {
            anyhow::anyhow!(
                "failed to hardlink image ref {} → {}: {e} \
                 (both paths must live on the same filesystem)",
                meta_path.display(),
                sandbox_image.display()
            )
        })?;
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

    let meta_path = image_dir.join("meta.json");
    let meta: ImageMeta = std::fs::read(&meta_path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "missing or corrupt {}",
                meta_path
                    .strip_prefix(image_dir)
                    .unwrap_or(&meta_path)
                    .display()
            )
        })?;
    if meta.layers.is_empty() {
        anyhow::bail!(
            "cached image at {} has no layer list — remove the sandbox's .airlock/sandbox/image \
             to force a re-pull",
            image_dir.display()
        );
    }

    let cfg = image_config.config.as_ref();
    let (uid, gid) = parse_user(cfg.and_then(|c| c.user.as_deref()).unwrap_or("0:0"));
    let container_home = lookup_home_dir(&meta.layers, uid)?;

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
        image_id: digest,
        image_layers: meta
            .layers
            .iter()
            .map(|d| crate::cache::digest_name(d).to_string())
            .collect(),
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

/// Ensure every layer is cached under `~/.cache/airlock/oci/layers/` and
/// persist the image's `meta.json` + `image_config.json`.
///
/// There is no merged `rootfs/` on the host anymore — the guest composes
/// overlayfs straight from the per-layer cache. Both registry and docker
/// paths converge on the same per-layer staging pipeline (see
/// [`layer::ensure_layer_cached`]).
async fn ensure_image(
    resolved: &mut ResolvedImage,
    image_name: &str,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<PathBuf> {
    let dir = crate::cache::image_dir(&resolved.digest)?;
    let meta_path = dir.join("meta.json");
    std::fs::create_dir_all(&dir)?;

    let ordered_layers = match &resolved.source {
        ImageSource::Docker { image_ref } => {
            let image_ref = image_ref.clone();
            let (cfg, layers) = ensure_docker_image(&image_ref, &dir)?;
            resolved.config = cfg;
            layers
        }
        ImageSource::Registry(reg) => {
            ensure_registry_image(reg, resolved, &dir, auth, insecure).await?
        }
    };

    let meta = ImageMeta {
        digest: resolved.digest.clone(),
        name: image_name.to_string(),
        layers: ordered_layers,
    };
    std::fs::write(&meta_path, serde_json::to_string_pretty(&meta)?)?;

    Ok(dir)
}

/// Stream `docker image save` and extract each referenced layer through the
/// shared per-layer cache. Returns the parsed image config plus layer
/// digests in topmost-first order.
fn ensure_docker_image(
    image_ref: &str,
    image_dir: &Path,
) -> anyhow::Result<(OciConfig, Vec<String>)> {
    let sp = cli::spinner("exporting from docker...");
    let save = docker::save_layer_tarballs(image_ref)?;
    std::fs::write(
        image_dir.join("image_config.json"),
        &save.image_config_bytes,
    )?;

    for digest in &save.layer_digests {
        layer::ensure_layer_cached(digest, |_tmp| {
            anyhow::bail!(
                "docker save stream did not include blob for layer {digest} \
                 (manifest referenced a layer that was not in the export)"
            )
        })?;
    }

    sp.finish_and_clear();
    cli::log!("  {} exported from docker", cli::check());

    // Docker save manifests are bottom-up; overlayfs wants topmost first.
    let mut ordered: Vec<String> = save
        .layer_digests
        .iter()
        .map(|d| crate::cache::digest_name(d).to_string())
        .collect();
    ordered.reverse();
    Ok((save.image_config, ordered))
}

/// Pull-and-extract for registry-sourced images. Layer downloads run
/// concurrently (bounded); each layer is streamed to its
/// `<digest>.download.tmp` path and extracted through the shared per-layer
/// cache. Returns layer digests in topmost-first order.
async fn ensure_registry_image(
    reg: &registry::RegistryImage,
    resolved: &ResolvedImage,
    image_dir: &Path,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<Vec<String>> {
    let layers = &reg.manifest.layers;

    let cached_count = layers
        .iter()
        .filter(|l| {
            crate::cache::layer_dir(&l.digest)
                .map(|p| p.join(".ok").exists())
                .unwrap_or(false)
        })
        .count();
    if cached_count > 0 {
        cli::log!(
            "  {} {} of {} layers found from cache",
            cli::check(),
            cached_count,
            layers.len()
        );
    }

    let to_fetch: Vec<usize> = layers
        .iter()
        .enumerate()
        .filter(|(_, l)| {
            !crate::cache::layer_dir(&l.digest)
                .map(|p| p.join(".ok").exists())
                .unwrap_or(false)
        })
        .map(|(i, _)| i)
        .collect();

    if !to_fetch.is_empty() {
        let mp = cli::multi_progress();
        let reference = &reg.reference;
        let mp_ref = &mp;

        let fetch = async {
            let mut stream = stream::iter(to_fetch.iter().copied())
                .map(|i| async move {
                    let layer_desc = &layers[i];
                    let label = format!("layer {:>2}", i + 1);
                    let per_layer = cli::layer_progress_bar(mp_ref, layer_desc.size as u64, &label);
                    let result =
                        fetch_and_extract_layer(reference, layer_desc, &per_layer, auth, insecure)
                            .await;
                    per_layer.finish_and_clear();
                    mp_ref.remove(&per_layer);
                    result
                })
                .buffer_unordered(3);

            while let Some(res) = stream.next().await {
                res?;
            }
            Ok::<(), anyhow::Error>(())
        };

        tokio::select! {
            res = fetch => { res?; }
            () = cli::interrupted() => {
                let _ = mp.clear();
                anyhow::bail!("interrupted");
            }
        }
        let _ = mp.clear();

        let downloaded_bytes: u64 = to_fetch.iter().map(|i| layers[*i].size as u64).sum();
        cli::log!(
            "  {} downloaded {}",
            cli::check(),
            cli::dim(&format!(
                "{} layers, {}",
                to_fetch.len(),
                format_size(downloaded_bytes as i64)
            ))
        );
    }

    let config_json = serde_json::to_string_pretty(&resolved.config)?;
    std::fs::write(image_dir.join("image_config.json"), config_json)?;

    // OCI manifests list layers bottom→top; overlayfs wants topmost first.
    let mut ordered: Vec<String> = layers
        .iter()
        .map(|l| crate::cache::digest_name(&l.digest).to_string())
        .collect();
    ordered.reverse();
    Ok(ordered)
}

/// Download one layer blob into `<digest>.download.tmp` and extract it
/// through the shared per-layer cache. `ensure_layer_cached` is a no-op
/// when `.ok` already exists, so the `to_fetch` filter in the caller is
/// a latency optimization, not a correctness requirement.
async fn fetch_and_extract_layer(
    reference: &oci_client::Reference,
    layer_desc: &oci_client::manifest::OciDescriptor,
    per_layer: &indicatif::ProgressBar,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<()> {
    let digest = layer_desc.digest.clone();
    let reference = reference.clone();
    let layer_desc = layer_desc.clone();
    let per_layer = per_layer.clone();
    let auth = auth.clone();

    // `ensure_layer_cached` does blocking I/O (tar extraction); keep it off
    // the async runtime. The fetch closure runs async code via a oneshot
    // channel trick — but simpler here: do the download on the blocking
    // thread by blocking on a oneshot from an async task. Instead, invert:
    // pull the blob async → write to .download.tmp → spawn blocking
    // extraction.
    let layers_root = crate::cache::layers_root()?;
    let hex = crate::cache::digest_name(&digest).to_string();
    let download = layers_root.join(format!("{hex}.download"));
    let download_tmp = layers_root.join(format!("{hex}.download.tmp"));

    // Short-circuit fast path identical to ensure_layer_cached.
    let layer_dir = crate::cache::layer_dir(&digest)?;
    if layer_dir.join(".ok").exists() && layer_dir.join("rootfs").is_dir() {
        return Ok(());
    }

    if !download.exists() {
        let _ = std::fs::remove_file(&download_tmp);
        registry::pull_layer(
            &reference,
            &layer_desc,
            &download_tmp,
            Some(&per_layer),
            None,
            &auth,
            insecure,
        )
        .await?;
        std::fs::rename(&download_tmp, &download)?;
    }

    tokio::task::spawn_blocking(move || {
        layer::ensure_layer_cached(&digest, |_tmp| {
            // .download already exists from the async pull above, so the
            // fetch closure is not called. If somehow it is, fail loudly.
            anyhow::bail!("unreachable: layer tarball missing after pull")
        })
    })
    .await??;
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

/// Look up a user's home directory by walking `/etc/passwd` in the per-layer
/// cache, topmost first. The first layer with a matching uid wins.
///
/// Reads from individual layer trees under `~/.cache/airlock/oci/layers/` —
/// the host no longer needs a merged `images/<d>/rootfs/` for this lookup.
/// Whiteouts manifest as empty files (which parse to zero matches and fall
/// through to the next layer); this is coarser than real overlayfs semantics
/// but is a safe superset for the common case of images that never delete
/// `/etc/passwd` in an upper layer.
fn lookup_home_dir(layer_digests: &[String], uid: u32) -> anyhow::Result<String> {
    for digest in layer_digests {
        let passwd_path = crate::cache::layer_dir(digest)?.join("rootfs/etc/passwd");
        let Ok(content) = std::fs::read_to_string(&passwd_path) else {
            continue;
        };
        for line in content.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if fields.len() >= 6 && fields[2].parse::<u32>().ok() == Some(uid) {
                return Ok(fields[5].to_string());
            }
        }
    }

    anyhow::bail!("no home directory found for uid {uid} in any layer /etc/passwd")
}
