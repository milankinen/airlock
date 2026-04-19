//! OCI image resolution, download, and extraction.
//!
//! Handles both Docker-daemon images and remote registry pulls, caches
//! layers locally, and returns an `OciImage` with all the metadata
//! needed by the VM to start the container.

mod credentials;
mod docker;
mod gc;
mod layer;
mod registry;

use std::path::Path;

use futures::stream::{self, StreamExt};
pub use gc::sweep as gc_sweep;
use oci_client::config::ConfigFile as OciConfig;
use oci_client::secrets::RegistryAuth;

use crate::cli;
use crate::oci::credentials::ToRegistryAuth;
use crate::project::Project;

/// Everything needed to configure the container process (returned by `prepare`).
/// Mount resolution, disk setup, and command/env overrides happen in `vm::start`.
///
/// Serialized to disk at `images/<digest>` wrapped in [`CachedImage`]; the
/// same file is hardlinked to `<sandbox>/image` as the GC liveness signal.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct OciImage {
    /// OCI image digest, used by supervisor to detect image changes.
    pub image_id: String,
    /// Image reference the user asked for (e.g. `alpine:3.20`). Stored so the
    /// fast path can confirm the cached entry still matches the project's
    /// configured image name without re-resolving the tag.
    pub name: String,
    /// Ordered layer digests — topmost-first. The guest mounts the shared
    /// `/mnt/layers/<digest>` cache as overlayfs lowerdirs in this order.
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
    let stored = read_cached_image(&project.sandbox_dir.join("image"));
    let image_cfg = &project.config.vm.image;
    let image_name = &image_cfg.name;

    // Fast path: if we already have a cached image for this name, skip
    // the network round-trip to resolve tag → digest. The image is
    // considered ready when every listed layer directory exists.
    if let Some(ref img) = stored
        && img.name == *image_name
        && !img.image_layers.is_empty()
        && img
            .image_layers
            .iter()
            .all(|d| crate::cache::layer_dir(d).is_ok_and(|p| p.is_dir()))
    {
        tracing::debug!("image cache hit for {image_name}");
        // Invariant: `sandbox/image` must be a hardlink to the canonical
        // cache file so sweep GC sees the sandbox as a live reference.
        // Heal it on every prepare — the link may have been severed by a
        // cache wipe, a cache-path migration, or a prior run that pre-dated
        // this invariant, leaving our entry as a sweep target.
        let image_path = crate::cache::image_path(&img.image_id)?;
        let sandbox_image = project.sandbox_dir.join("image");
        ensure_image_hardlink(&sandbox_image, &image_path, img)?;
        cli::log!(
            "  {} image cached {}",
            cli::check(),
            cli::dim(&img.image_id[..19.min(img.image_id.len())])
        );
        let overlay_dir = project.sandbox_dir.join("overlay");
        std::fs::create_dir_all(&overlay_dir)?;
        cli::log!("  {} environment ready", cli::check());
        return Ok(stored.unwrap());
    }

    let stored_digest = stored.map(|i| i.image_id);

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
    let digest_changed = stored_digest
        .as_deref()
        .is_none_or(|s| s.trim() != image.digest);

    if let Some(old_digest) = stored_digest
        && digest_changed
    {
        match prompt_image_changed()? {
            ImageChangeAction::KeepOld => {
                // Only keep old if the cache is still intact; otherwise
                // fall through and let the new image be used.
                let old_image_path = crate::cache::image_path(old_digest.trim())?;
                if old_image_path.exists() {
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
    let oci_image = ensure_image(&mut image, image_name, &auth, image_cfg.insecure).await?;

    // Hard-link the cached image file into the sandbox directory. nlink > 1
    // on `images/<digest>` is the GC guard — without it, a sibling sandbox
    // creating a new image could trigger a sweep that wrongly deletes this
    // one. Enforce unconditionally: even when the digest hasn't changed, a
    // previous run may have left the sandbox with a standalone copy instead
    // of a hardlink.
    let image_path = crate::cache::image_path(&oci_image.image_id)?;
    let sandbox_image = project.sandbox_dir.join("image");
    ensure_image_hardlink(&sandbox_image, &image_path, &oci_image)?;

    let overlay_dir = project.sandbox_dir.join("overlay");
    std::fs::create_dir_all(&overlay_dir)?;
    cli::log!("  {} environment ready", cli::check());

    Ok(oci_image)
}

/// On-disk wrapper for a cached [`OciImage`]. Internally tagged so the JSON
/// carries `"schema":"v1"` alongside the image fields — future schema bumps
/// just add a new variant and `serde` picks the right one based on the tag.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "schema")]
enum CachedImage {
    #[serde(rename = "v1")]
    V1(OciImage),
}

/// Read a cached image JSON file and unwrap it into an [`OciImage`]. Returns
/// `None` when the file is absent, unreadable, or written by an
/// unrecognized schema version — callers treat any of those as a cache miss.
fn read_cached_image(path: &Path) -> Option<OciImage> {
    let data = std::fs::read(path).ok()?;
    let wrapped: CachedImage = serde_json::from_slice(&data).ok()?;
    let CachedImage::V1(image) = wrapped;
    Some(image)
}

/// Ensure `sandbox_image` is a hardlink to `images/<digest>` — the GC
/// liveness signal. No-op when the inodes already match; otherwise severs
/// the old `sandbox_image` and links it fresh. If the canonical cache file
/// is missing (cache wipe, path migration), the sandbox copy is written
/// back out first so we have something to link to.
///
/// Fails hard on link error because the only plausible cause is a
/// cross-filesystem config problem (both paths are under `$HOME`), and
/// silently falling through would leave the sandbox un-GC-protected.
fn ensure_image_hardlink(
    sandbox_image: &Path,
    image_path: &Path,
    image: &OciImage,
) -> anyhow::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let linked = match (
        std::fs::metadata(sandbox_image),
        std::fs::metadata(image_path),
    ) {
        (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
        _ => false,
    };
    if linked {
        return Ok(());
    }
    if !image_path.exists() {
        write_cached_image(image_path, image)?;
    }
    let _ = std::fs::remove_file(sandbox_image);
    std::fs::hard_link(image_path, sandbox_image).map_err(|e| {
        anyhow::anyhow!(
            "failed to hardlink image ref {} → {}: {e} \
             (both paths must live on the same filesystem)",
            image_path.display(),
            sandbox_image.display()
        )
    })?;
    Ok(())
}

/// Write a cached image atomically: serialize, write to `<path>.tmp`, rename.
/// Rename is the commit point, same idiom as the layer cache.
fn write_cached_image(path: &Path, image: &OciImage) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(&CachedImage::V1(image.clone()))?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Bake the parsed OCI image config plus the ordered layer list into an
/// `OciImage`: extracts uid/gid, merges entrypoint+cmd, applies env defaults,
/// and resolves `$HOME` from the per-layer `/etc/passwd`.
fn build_oci_image(
    image_id: String,
    name: String,
    ordered_layers: Vec<String>,
    image_config: &OciConfig,
) -> anyhow::Result<OciImage> {
    if ordered_layers.is_empty() {
        anyhow::bail!("image {image_id} has no layers");
    }

    let cfg = image_config.config.as_ref();
    let (uid, gid) = parse_user(cfg.and_then(|c| c.user.as_deref()).unwrap_or("0:0"));
    let container_home = lookup_home_dir(&ordered_layers, uid)?;

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
        image_id,
        name,
        image_layers: ordered_layers,
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

/// Ensure every layer is cached under `~/.cache/airlock/oci/layers/`, bake
/// the image metadata into an [`OciImage`], and persist it as a single
/// schema-tagged JSON file at `images/<digest>`.
///
/// There is no merged rootfs on the host — the guest composes overlayfs
/// straight from the per-layer cache. Both registry and docker paths
/// converge on the same per-layer staging pipeline (see
/// [`layer::ensure_layer_cached`]).
async fn ensure_image(
    resolved: &mut ResolvedImage,
    image_name: &str,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<OciImage> {
    let image_path = crate::cache::image_path(&resolved.digest)?;

    // Digest-keyed cache hit: a sibling project already pulled this exact
    // image and all its layers are still on disk. Skip the source-specific
    // pull entirely. We refresh the stored name so the per-sandbox fast
    // path in `prepare()` (which matches on name) sees the current tag.
    if let Some(mut cached) = read_cached_image(&image_path)
        && !cached.image_layers.is_empty()
        && cached
            .image_layers
            .iter()
            .all(|d| crate::cache::layer_dir(d).is_ok_and(|p| p.is_dir()))
    {
        if cached.name != image_name {
            cached.name = image_name.to_string();
            write_cached_image(&image_path, &cached)?;
        }
        return Ok(cached);
    }

    let ordered_layers = match &resolved.source {
        ImageSource::Docker { image_ref } => {
            let image_ref = image_ref.clone();
            let (cfg, layers) = ensure_docker_image(&image_ref)?;
            resolved.config = cfg;
            layers
        }
        ImageSource::Registry(reg) => ensure_registry_image(reg, auth, insecure).await?,
    };

    let image = build_oci_image(
        resolved.digest.clone(),
        image_name.to_string(),
        ordered_layers,
        &resolved.config,
    )?;
    write_cached_image(&image_path, &image)?;
    Ok(image)
}

/// Stream `docker image save` and extract each referenced layer through the
/// shared per-layer cache. Returns the parsed image config plus layer
/// digests in topmost-first order.
fn ensure_docker_image(image_ref: &str) -> anyhow::Result<(OciConfig, Vec<String>)> {
    let sp = cli::spinner("exporting from docker...");
    let save = docker::save_layer_tarballs(image_ref)?;

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
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<Vec<String>> {
    let layers = &reg.manifest.layers;

    let cached_count = layers
        .iter()
        .filter(|l| {
            crate::cache::layer_dir(&l.digest)
                .map(|p| p.is_dir())
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
                .map(|p| p.is_dir())
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
/// when the layer dir already exists, so the `to_fetch` filter in the
/// caller is a latency optimization, not a correctness requirement.
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
    if layer_dir.is_dir() {
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
/// the host has no merged rootfs to consult for this lookup. Whiteouts
/// manifest as empty files (which parse to zero matches and fall through
/// to the next layer); this is coarser than real overlayfs semantics but
/// is a safe superset for the common case of images that never delete
/// `/etc/passwd` in an upper layer.
fn lookup_home_dir(layer_digests: &[String], uid: u32) -> anyhow::Result<String> {
    for digest in layer_digests {
        let passwd_path = crate::cache::layer_dir(digest)?.join("etc/passwd");
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
