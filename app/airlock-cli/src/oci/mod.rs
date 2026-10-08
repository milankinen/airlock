//! OCI image support.
//!
//! Gets the container image of a sandbox ready for the VM. The image can come
//! from a local Docker or Podman engine or from a remote registry. The host
//! keeps downloaded images and layers in a shared cache.
//!
//! Also supports:
//!  * reading files from an image, for example the OS release information
//!  * running the user's command in a login shell
//!  * removing cached images and layers that no sandbox uses

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

use crate::cli::prompt::PromptError;
use crate::cli::prompt::choose::{Choice, Choose};
use crate::cli::prompt::style::Tone;
use crate::config::config_values::{ImageRef, PullPolicy};
use crate::oci::credentials::ToRegistryAuth;
use crate::project::Project;
use crate::vault::Vault;
use crate::{cache, cli};

/// The `PATH` of a container whose image sets none.
pub const DEFAULT_PATH: &str = "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin";

/// Largest file that the host reads from a layer (for example `etc/passwd`,
/// `etc/group` or `etc/os-release`). Real files are a few KB. A larger file
/// counts as "no records" and is not read, so an image cannot control how
/// much memory these reads use.
const MAX_LAYER_RECORD_FILE: u64 = 1024 * 1024;

/// Image metadata that configures the container process. [`prepare`]
/// returns it.
///
/// This type has no mounts, disk setup, `[env]` overrides or command
/// overrides. `vm::start` adds mounts, disks and env (from the resolved
/// `SandboxEnv`). `sandbox::main_argv` adds command overrides.
///
/// Stored on disk at `images/<digest>`, wrapped in [`CachedImage`]. The same
/// file is hardlinked to `<sandbox>/image` to tell GC that the image is in use.
#[derive(serde::Serialize, serde::Deserialize, Clone)]
pub struct OciImage {
    /// OCI image digest. The supervisor uses it to detect image changes.
    pub image_id: String,
    /// Image reference that the user configured (e.g. `alpine:3.20`). The
    /// fast path uses it to check that the cached entry still matches the
    /// configured image name, without a new tag resolution.
    pub name: String,
    /// Layer keys, topmost first. Each key is a versioned layer name
    /// ([`cache::layer_key`]). It is the directory name under
    /// `<data>/oci/layers/<key>` and the guest mount path
    /// `/mnt/layers/<key>`.
    pub image_layers: Vec<String>,
    /// Container home directory from the image's user record (e.g. `/root`).
    /// For `~` expansion of guest paths, use [`effective_container_home`].
    /// The user can override `HOME` in `[env]`, and `~` must then expand to
    /// the same path as `$HOME` in the sandbox.
    pub container_home: String,
    /// Container uid (from image config).
    pub uid: u32,
    /// Container gid (from image config).
    pub gid: u32,
    /// Image entrypoint and cmd merged. `/bin/sh` if both are empty.
    /// Has no command overrides (`sandbox::main_argv` adds them).
    pub cmd: Vec<String>,
    /// Base defaults (`PATH`, `TERM`, `HOME`) and the image env.
    /// Has no `[env]` overrides (`vm::start` adds them from `SandboxEnv`).
    pub env: Vec<String>,
    /// The raw `USER` string from the image config (`""` if the image sets
    /// none). `None` marks a file written before airlock resolved named users
    /// through the image's `/etc/passwd`
    /// (<https://github.com/milankinen/airlock/pull/12>). Such a file can
    /// contain root `uid`/`gid` for an image that asked for a non-root user,
    /// so [`prepare`] checks it again against a new resolution before use.
    #[serde(default)]
    pub user: Option<String>,
}

/// What [`prepare`] does when the configured image resolves to a digest
/// that is different from the digest the sandbox was created with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnImageChange {
    /// Ask the user to re-create the sandbox, continue with the current one,
    /// or cancel.
    Ask,
    /// Re-create the sandbox without a question (`--yes`).
    Recreate,
    /// Stop with [`ImageChangeStop::NeedsTerminal`] because nobody can answer.
    Refuse,
}

/// What [`prepare`] did with the image of the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageChange {
    /// The image is the one the sandbox was created with (or the sandbox
    /// had no image yet).
    Unchanged,
    /// The image changed and the sandbox must be re-created. The caller
    /// deletes the sandbox disk and the install records.
    Recreate,
    /// The image changed, and the sandbox continues with its old image.
    KeepOld,
    /// The image changed and the sandbox was to continue with its old
    /// image, but the old image is not complete in the cache any more.
    /// The sandbox continues with the new image.
    OldImageGone,
}

/// Why [`prepare`] stopped early. A typed error inside the `anyhow::Error`.
#[derive(Debug, thiserror::Error)]
pub enum ImageChangeStop {
    /// There is no terminal for the question, and no `--yes`.
    #[error("Sandbox image has been changed. Run in a terminal or pass --yes.")]
    NeedsTerminal,
    /// The user chose Cancel (or pressed Esc).
    #[error("cancelled by user")]
    Cancelled,
    /// Ctrl+C during the question or the image download.
    #[error("interrupted")]
    Interrupted,
}

/// The prepared image of a sandbox, and what happened to the old one.
pub struct PreparedImage {
    /// The image that the sandbox uses.
    pub image: OciImage,
    /// What [`prepare`] did with the image of the sandbox.
    pub change: ImageChange,
}

/// Resolve, download and prepare the OCI image for a sandbox.
/// Args:
///  - `sandbox_dir`: Directory of the sandbox
///  - `image_cfg`: Configured image reference and pull settings
///  - `vault`: Vault that holds the registry credentials
///  - `on_change`: What to do if the image digest changed since the sandbox
///    was created.
///
/// Returns:
///   The prepared image and what happened to the old image, or error.
///   Stops with [`ImageChangeStop`] if the user cancels or nobody can answer.
pub async fn prepare(
    sandbox_dir: &Path,
    image_cfg: &ImageRef,
    vault: &Vault,
    on_change: OnImageChange,
) -> anyhow::Result<PreparedImage> {
    let sandbox_image = sandbox_dir.join("image");
    let image_name = &image_cfg.name;
    let unchanged = |image| PreparedImage {
        image,
        change: ImageChange::Unchanged,
    };

    // The cached image of this sandbox. Set only if it is complete on disk
    // and its name is still the configured image name.
    let cached = read_ready_image(&sandbox_image).filter(|img| img.name == *image_name);

    // A file without `user` is older than named-USER resolution and can
    // contain a wrong uid/gid. The code below must check it again, so it
    // never takes the fast path and is never a digest-keyed cache hit.
    let legacy = cached.as_ref().is_some_and(|img| img.user.is_none());

    // Fast path: use the cached image and skip the network request that
    // resolves the tag to a digest.
    //
    // `if-changed` does not use this shortcut on purpose. The network request
    // is the purpose of that policy. A digest-pinned reference always takes
    // the fast path: it names one immutable image, so a matching name means
    // a matching digest and there is nothing to detect.
    if let Some(img) = cached.clone()
        && !legacy
        && (image_cfg.pull_policy == PullPolicy::IfNotPresent
            || image_cfg.pinned_digest().is_some())
    {
        tracing::debug!("image cache hit for {image_name}");
        return use_cached_image(sandbox_dir, &sandbox_image, img).map(unchanged);
    }

    // Read only the stored digest (if any) for change detection.
    let stored_digest = read_cached_image(&sandbox_image).map(|i| i.image_id);

    let registry_host: String = image_name
        .parse::<oci_client::Reference>()
        .map_or_else(|_| image_name.clone(), |r| r.resolve_registry().to_string());

    let (mut image, auth) = match resolve_with_auth(vault, image_cfg, &registry_host).await {
        Ok(resolved) => resolved,
        Err(e) => {
            // With `if-changed`, airlock contacted the source only to ask if
            // a newer digest exists. If the registry is down or unreachable,
            // the sandbox must not stop when its image is already complete
            // on disk. Offer to continue with the cached image.
            let Some(img) = cached.filter(|_| !cli::is_interrupted()) else {
                return Err(e);
            };
            if !prompt_resolution_failed(&e)? {
                return Err(e);
            }
            return use_cached_image(sandbox_dir, &sandbox_image, img).map(unchanged);
        }
    };

    // Same digest, but the old `USER` resolution made the cached metadata.
    // Find uid/gid again from the new config. Mark the file as verified, or
    // refuse to start the sandbox if it ran as the wrong user.
    if let Some(img) = cached
        .as_ref()
        .filter(|i| i.user.is_none() && i.image_id == image.digest)
    {
        verify_legacy_user(img, &image)?;
    }

    // Check for an image change before the download.
    let digest_changed = stored_digest
        .as_deref()
        .is_none_or(|s| s.trim() != image.digest);

    let mut change = ImageChange::Unchanged;
    if let Some(old_digest) = stored_digest
        && digest_changed
    {
        change = match on_change {
            OnImageChange::Ask => ask_image_changed()?,
            OnImageChange::Recreate => ImageChange::Recreate,
            OnImageChange::Refuse => return Err(ImageChangeStop::NeedsTerminal.into()),
        };
        if change == ImageChange::KeepOld {
            // The old image must be *ready*, not only present. A sweep can
            // remove the layer trees and keep the JSON. With the old digest,
            // `ensure_image` then pulls the **new** image into the **old**
            // digest entry. This corrupts the entry for all sandboxes that
            // share it, and the `image_id` describes neither image.
            // A return here makes this mismatch impossible. A digest gets to
            // `ensure_image` only from the same resolution as its source.
            let old_image_path = crate::cache::image_path(old_digest.trim())?;
            if let Some(mut old) = read_ready_image(&old_image_path) {
                // Write the configured name into the kept image, so the
                // name-keyed fast path finds it on the next start. Otherwise
                // each later run resolves again and asks the same question.
                if old.name != *image_name {
                    old.name.clone_from(image_name);
                    write_cached_image(&old_image_path, &old)?;
                }
                return use_cached_image(sandbox_dir, &sandbox_image, old).map(|image| {
                    PreparedImage {
                        image,
                        change: ImageChange::KeepOld,
                    }
                });
            }
            change = ImageChange::OldImageGone;
            cli::log!(
                "  {} the old image is not available any more — using the new image",
                cli::bullet()
            );
        } else {
            // Remove the image hardlink. This sandbox then no longer marks
            // the old image as in use, so the sweep below can collect it.
            let _ = std::fs::remove_file(&sandbox_image);
            cli::log!("  {} old environment erased", cli::check());
            // GC: remove images that no sandbox uses, and the layers that
            // only those images used.
            gc::sweep();
        }
    }

    let oci_image = tokio::select! {
        res = ensure_image(&mut image, image_name, &auth, image_cfg.insecure) => res?,
        () = cli::interrupted() => return Err(ImageChangeStop::Interrupted.into()),
    };
    // Hardlink the cached image file into the sandbox directory. nlink > 1
    // on `images/<digest>` protects it from GC. Without it, a sibling sandbox
    // that creates a new image can start a sweep that deletes this one.
    // Always do this: also when the digest did not change, a previous run
    // can have left a standalone copy instead of a hardlink.
    let image_path = crate::cache::image_path(&oci_image.image_id)?;
    ensure_image_hardlink(&sandbox_image, &image_path, &oci_image)?;

    let overlay_dir = sandbox_dir.join("overlay");
    std::fs::create_dir_all(&overlay_dir)?;
    cli::log!("  {} environment ready", cli::check());

    Ok(PreparedImage {
        image: oci_image,
        change,
    })
}

/// Finish [`prepare`] with an image that is already cached on disk. Creates
/// the GC hardlink again, makes sure that the overlay dir exists, and reports
/// the image as ready.
///
/// The fast path and the resolution-failure fallback both use this, so both
/// leave the sandbox in the same state as a newly pulled image.
fn use_cached_image(
    sandbox_dir: &Path,
    sandbox_image: &Path,
    image: OciImage,
) -> anyhow::Result<OciImage> {
    // Invariant: `sandbox/image` must be a hardlink to the canonical cache
    // file, so the GC sweep sees that the sandbox uses it. Repair it on each
    // prepare. A cache wipe, a cache-path migration, or an older run can
    // break the link. Then the sweep can delete the image.
    let image_path = crate::cache::image_path(&image.image_id)?;
    ensure_image_hardlink(sandbox_image, &image_path, &image)?;
    cli::log!(
        "  {} image cached {}",
        cli::check(),
        cli::dim(&image.image_id[..19.min(image.image_id.len())])
    );
    let overlay_dir = sandbox_dir.join("overlay");
    std::fs::create_dir_all(&overlay_dir)?;
    cli::log!("  {} environment ready", cli::check());
    Ok(image)
}

/// Resolve the configured image reference to a digest and find working
/// registry auth.
/// Args:
///  - `vault`: Vault that stores the registry credentials
///  - `image_cfg`: Configured image reference
///  - `registry_host`: Registry host for the credential lookup.
///
/// Returns:
///   The resolved image and the auth that worked, so the caller can use the
///   same auth for the pull.
async fn resolve_with_auth(
    vault: &Vault,
    image_cfg: &ImageRef,
    registry_host: &str,
) -> anyhow::Result<(ResolvedImage, RegistryAuth)> {
    // Try anonymous first, then the credentials in the vault, then ask the
    // user. Try again until resolution succeeds or the user interrupts.
    let mut auth = RegistryAuth::Anonymous;
    let mut updated_creds = None;
    loop {
        match resolve_image(image_cfg, &auth).await {
            Ok(img) => {
                if let Some(creds) = updated_creds {
                    credentials::save(vault, registry_host, &creds)?;
                }
                return Ok((img, auth));
            }
            Err(e) if registry::is_auth_error(&e) => {
                if cli::is_interrupted() {
                    anyhow::bail!("cancelled by user");
                }
                if auth == RegistryAuth::Anonymous
                    && let Some(creds) = credentials::load(vault, registry_host)
                {
                    auth = creds.to_auth();
                    continue;
                }
                cli::error!("authentication failed, try again");
                let creds = credentials::prompt(registry_host)?;
                updated_creds = Some(creds.clone());
                auth = creds.to_auth();
            }
            Err(e) => return Err(e),
        }
    }
}

/// On-disk wrapper for a cached [`OciImage`]. Internally tagged, so the JSON
/// has `"schema":"v2"` next to the image fields. The schema version changes
/// together with [`crate::cache::LAYER_FORMAT`]. Thus after a layer format
/// change, all old image JSON files fail to deserialize, which causes a
/// clean pull.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "schema")]
enum CachedImage {
    #[serde(rename = "v2")]
    V2(OciImage),
}

/// Read a cached image JSON file and unwrap it into an [`OciImage`].
/// Returns:
///   The image, or `None` if the file is missing, cannot be read, or has an
///   unknown schema version. Callers treat `None` as a cache miss.
fn read_cached_image(path: &Path) -> Option<OciImage> {
    let data = std::fs::read(path).ok()?;
    let wrapped: CachedImage = serde_json::from_slice(&data).ok()?;
    let CachedImage::V2(image) = wrapped;
    Some(image)
}

/// Like [`read_cached_image`], but all layers of the image must also exist
/// on disk.
/// Returns:
///   The image, or `None` if the file is missing or a sweep removed a layer.
///   In both cases the caller must resolve the image again.
fn read_ready_image(path: &Path) -> Option<OciImage> {
    let image = read_cached_image(path)?;
    if image.image_layers.is_empty() {
        return None;
    }
    image
        .image_layers
        .iter()
        .all(|k| cache::layer_dir(k).is_ok_and(|p| p.is_dir()))
        .then_some(image)
}

/// Make sure that `sandbox_image` is a hardlink to `images/<digest>`. The
/// hardlink tells GC that the sandbox uses the image.
/// Args:
///  - `sandbox_image`: The `image` file in the sandbox directory
///  - `image_path`: The canonical cache file `images/<digest>`
///  - `image`: Image to write if the cache file is missing.
///
/// Returns:
///   Error if the hardlink fails.
fn ensure_image_hardlink(
    sandbox_image: &Path,
    image_path: &Path,
    image: &OciImage,
) -> anyhow::Result<()> {
    // Do nothing if the inodes already match. Otherwise remove the old
    // `sandbox_image` and link it again.
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
    // A cache wipe or a path migration can remove the canonical cache file.
    // Write the sandbox copy back first, so there is a file to link to.
    if !image_path.exists() {
        write_cached_image(image_path, image)?;
    }
    let _ = std::fs::remove_file(sandbox_image);
    // Fail on a link error. Both paths are under `$HOME`, so the only
    // probable cause is a cross-filesystem config problem. Without the link,
    // GC does not protect the sandbox image.
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

/// Write a cached image file atomically.
fn write_cached_image(path: &Path, image: &OciImage) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write to `<path>.tmp`, then rename. The rename is the commit point,
    // the same as in the layer cache.
    let tmp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(&CachedImage::V2(image.clone()))?;
    std::fs::write(&tmp, &bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Make an [`OciImage`] from the parsed OCI image config and the layer list.
/// Args:
///  - `image_id`: Image digest
///  - `name`: Configured image reference
///  - `ordered_layers`: Layer keys, topmost first
///  - `image_config`: Parsed OCI image config.
///
/// Returns:
///   The image with uid/gid, command, env and home directory, or error if
///   the image has no layers, or its user or home directory cannot be
///   resolved.
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
    let user = cfg.and_then(|c| c.user.as_deref()).unwrap_or("");
    let (uid, gid) = resolve_user(&ordered_layers, user)?;
    let container_home = lookup_home_dir(&ordered_layers, uid)?;

    // Container command: entrypoint and cmd merged.
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

    // Environment: base defaults, then image env. No sandbox overrides here.
    let host_term = std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".to_string());
    let mut env: Vec<String> = vec![
        format!("PATH={DEFAULT_PATH}"),
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
        user: Some(user.to_string()),
        cmd,
        env,
    })
}

/// Resolve the home directory that `~` expands to in guest paths.
/// Args:
///  - `project`: Project with the resolved `[env]`
///  - `image`: The prepared image.
///
/// Returns:
///   The `HOME` value from `[env]` as the guest sees it, or the home
///   directory from the image's user record if `[env]` does not set `HOME`.
pub fn effective_container_home(project: &Project, image: &OciImage) -> String {
    // `~` must expand to the same path as `$HOME` in the sandbox. Example:
    // a `target = "~/foo"` mount with `[env].HOME = "/x"`. If `~` expands
    // to the image home (`/root`), the shell sees `$HOME` as `/x`. Paths
    // then go to the wrong place, and tools that convert a mount path back
    // to `~` form do not match the real mount.
    //
    // `SandboxEnv::resolve` already did the `${VAR}` substitution (in
    // `project::open` or `Project::with_config`).
    project
        .env
        .guest_value("HOME")
        .map_or_else(|| image.container_home.clone(), str::to_string)
}

/// Wrap a command so that it runs in a login shell.
///
/// A lone shell binary (`sh`, `bash`, etc.) gets `-l` at the end. Other
/// commands become `bash -l -c 'exec "$0" "$@"' cmd args...`, which passes
/// the arguments without quoting.
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

/// Resolve an image `USER` string to a numeric uid and gid.
///
/// The OCI image spec allows `user`, `uid`, `user:group`, `uid:gid`,
/// `uid:group` and `user:gid`. Names are looked up in the image's own
/// `/etc/passwd` and `/etc/group` (with [`lookup_layer_record`]), the same
/// as Docker does. A user without a group part gets the primary gid of
/// that user from `passwd`. An empty string means root, the same as for an
/// image that does not set `USER`.
///
/// A name that no layer declares is an error, not a fallback to root. The
/// image author wrote `USER` to prevent root access for an unprivileged
/// user, so a silent change to root is not acceptable.
/// Args:
///  - `layer_keys`: Layer keys, topmost first
///  - `user`: The `USER` string from the image config.
///
/// Returns:
///   The `(uid, gid)` pair, or error if a name is not found.
fn resolve_user(layer_keys: &[String], user: &str) -> anyhow::Result<(u32, u32)> {
    let (user_part, group_part) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };

    // Each passwd record is `name:pw:uid:gid:gecos:home:shell`. A match
    // gives `(uid, primary gid)`.
    let passwd_record = |matches: &dyn Fn(&[&str]) -> bool| {
        lookup_layer_record(layer_keys, "etc/passwd", |f| {
            if f.len() >= 4 && matches(f) {
                Some((f[2].parse::<u32>().ok()?, f[3].parse::<u32>().ok()?))
            } else {
                None
            }
        })
    };

    let (uid, primary_gid) = if user_part.is_empty() {
        (0, None)
    } else if let Ok(uid) = user_part.parse::<u32>() {
        let record = passwd_record(&|f| f[2].parse::<u32>().ok() == Some(uid))?;
        (uid, record.value.map(|(_, gid)| gid))
    } else {
        let (uid, gid) = passwd_record(&|f| f[0] == user_part)?
            .ok_or_else(|| format!("no user {user_part} found in any layer /etc/passwd"))?;
        (uid, Some(gid))
    };

    let gid = match group_part {
        None | Some("") => primary_gid.unwrap_or(0),
        Some(g) => resolve_group(layer_keys, g)?,
    };

    Ok((uid, gid))
}

/// Resolve the group part of a `USER` string. A numeric gid is used as it
/// is. A name is looked up in the image's `/etc/group`.
fn resolve_group(layer_keys: &[String], group: &str) -> anyhow::Result<u32> {
    if let Ok(gid) = group.parse::<u32>() {
        return Ok(gid);
    }
    // Each group record is `name:pw:gid:members`.
    lookup_layer_record(layer_keys, "etc/group", |f| {
        if f.len() >= 3 && f[0] == group {
            f[2].parse::<u32>().ok()
        } else {
            None
        }
    })?
    .ok_or_else(|| format!("no group {group} found in any layer /etc/group"))
}

/// Ask the user what to do with the changed image of an existing sandbox.
/// Esc and Cancel stop with [`ImageChangeStop::Cancelled`].
fn ask_image_changed() -> anyhow::Result<ImageChange> {
    let question = Choose {
        title: "Sandbox image has been changed",
        notes: &[],
        choices: &[
            Choice {
                label: "re-create sandbox",
                tone: Tone::Plain,
            },
            Choice {
                label: "continue with current sandbox",
                tone: Tone::Plain,
            },
            Choice {
                label: "cancel",
                tone: Tone::Plain,
            },
        ],
        default: 0,
        report: true,
    };
    let choice = question.ask().map_err(|e| match e {
        PromptError::NotInteractive => anyhow::Error::from(ImageChangeStop::NeedsTerminal),
        PromptError::Interrupted => ImageChangeStop::Interrupted.into(),
        PromptError::Io(e) => e.into(),
    })?;
    match choice {
        Some(0) => Ok(ImageChange::Recreate),
        Some(1) => Ok(ImageChange::KeepOld),
        _ => Err(ImageChangeStop::Cancelled.into()),
    }
}

/// Ask the user if airlock uses the cached image after a resolution failure.
/// Returns:
///   `true` to continue with the cached image. `false` to stop (the
///   default, Esc, or no terminal).
fn prompt_resolution_failed(err: &anyhow::Error) -> anyhow::Result<bool> {
    // Non-interactive runs stop. A resolution failure is a real error, and a
    // silent change to a possibly old image in CI would hide it.
    if !cli::is_interactive() {
        return Ok(false);
    }
    let title = format!("Image resolution failed: {err}");
    let question = Choose {
        title: &title,
        notes: &[],
        choices: &[
            Choice {
                label: "cancel",
                tone: Tone::Plain,
            },
            Choice {
                label: "continue with the cached image",
                tone: Tone::Plain,
            },
        ],
        default: 0,
        report: false,
    };
    match question.ask() {
        Ok(choice) => Ok(choice == Some(1)),
        Err(PromptError::NotInteractive) => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// Result of a new check of the uid/gid of a legacy cache file against a new
/// resolution of the image's `USER`.
#[derive(Debug, PartialEq, Eq)]
enum LegacyUser {
    /// The stored uid/gid are the same as the fixed resolution gives.
    Verified,
    /// Same uid, different primary group (`USER 1000` used to get gid 0).
    /// Existing files still have the same owner, so airlock repairs the
    /// cache entry and keeps the sandbox.
    GidOnly { gid: u32 },
    /// The sandbox ran as the wrong user (usually root for `USER node`), and
    /// its disk contains state that this user owns.
    UidMismatch { uid: u32, gid: u32 },
}

/// Find uid/gid again for a legacy cache file (one without `user`). Uses
/// the current `USER` string and the layers that the file refers to.
fn check_legacy_user(stored: &OciImage, user: &str) -> anyhow::Result<LegacyUser> {
    let (uid, gid) = resolve_user(&stored.image_layers, user)?;
    Ok(if uid != stored.uid {
        LegacyUser::UidMismatch { uid, gid }
    } else if gid != stored.gid {
        LegacyUser::GidOnly { gid }
    } else {
        LegacyUser::Verified
    })
}

/// Get the `USER` string of a newly resolved image. Registry resolution
/// already has the config. Local resolution gets the config only at the
/// export, so ask the engine directly.
fn resolved_user(resolved: &ResolvedImage) -> anyhow::Result<String> {
    match &resolved.source {
        ImageSource::Registry(_) => Ok(resolved
            .config
            .config
            .as_ref()
            .and_then(|c| c.user.clone())
            .unwrap_or_default()),
        ImageSource::Local { engine, .. } => docker::image_user(engine, &resolved.digest),
    }
}

/// Check the uid/gid of a legacy cached image against the fixed `USER`
/// resolution.
/// Args:
///  - `stored`: Cached image of this sandbox, written before named `USER`
///    resolution existed. It has the same digest as `resolved`
///  - `resolved`: The newly resolved image.
///
/// Returns:
///   `Ok` after it marks the file as verified or repairs its gid. Error that
///   tells the user to run `airlock rm` if the uid is wrong.
fn verify_legacy_user(stored: &OciImage, resolved: &ResolvedImage) -> anyhow::Result<()> {
    let user = resolved_user(resolved)?;
    let mut fixed = stored.clone();
    fixed.user = Some(user.clone());
    match check_legacy_user(stored, &user)? {
        LegacyUser::Verified => {}
        LegacyUser::GidOnly { gid } => {
            cli::log!(
                "  {} container gid corrected {} → {gid}",
                cli::bullet(),
                stored.gid
            );
            fixed.gid = gid;
        }
        LegacyUser::UidMismatch { uid, gid } => {
            // After `airlock rm`, the next start finds no sandbox image, and
            // `ensure_image` makes the shared entry again instead of using
            // the legacy one.
            anyhow::bail!(
                "This sandbox was created by an airlock version that resolved the \
                 image's `USER {user}` to uid {}, gid {} instead of uid {uid}, gid {gid}, \
                 so everything inside it has been running as the wrong user. That bug \
                 is fixed (https://github.com/milankinen/airlock/pull/12), but the \
                 sandbox's disk already holds state owned by the wrong user, so the \
                 sandbox must be re-created: run `airlock rm` and start again.",
                stored.uid,
                stored.gid
            );
        }
    }
    // Write the shared cache entry again. `prepare` links the sandbox copy
    // to the new file with `ensure_image_hardlink`.
    write_cached_image(&crate::cache::image_path(&stored.image_id)?, &fixed)
}

/// Resolve the configured image to a digest and config. Tries the local
/// engines first (as `resolution` allows), then the registry.
async fn resolve_image(
    image_cfg: &crate::config::config_values::ImageRef,
    auth: &RegistryAuth,
) -> anyhow::Result<ResolvedImage> {
    use crate::config::config_values::Resolution;

    let image_ref = image_cfg.name.as_str();
    let pinned = image_cfg.pinned_digest();

    let engines: &[&str] = match image_cfg.resolution {
        Resolution::Auto => &["docker", "podman"],
        Resolution::Docker => &["docker"],
        Resolution::Podman => &["podman"],
        Resolution::Registry => &[],
    };
    let local_only = matches!(
        image_cfg.resolution,
        Resolution::Docker | Resolution::Podman
    );
    for engine in engines {
        match resolve_local(engine, image_ref, pinned) {
            Ok(Some(resolved)) => return Ok(resolved),
            // The image is local but not usable. With a single engine there
            // is no other source, so show the reason, not a generic
            // "not found".
            Err(reason) if local_only => anyhow::bail!("{reason}"),
            Err(reason) => cli::log!("  {} {reason} — trying next", cli::bullet()),
            Ok(None) => {}
        }
    }
    if local_only {
        anyhow::bail!("image {image_ref} not found in {}", engines[0]);
    }

    let reg = registry::resolve(image_ref, auth, image_cfg.insecure).await?;
    // A pin can name the multi-platform index or the platform manifest from
    // it. Users can correctly copy both from a registry.
    if let Some(want) = pinned
        && reg.digest != want
        && reg.list_digest.as_deref() != Some(want)
    {
        anyhow::bail!(
            "registry resolved {image_ref} to digest {}, which does not match the pinned {want}",
            reg.digest
        );
    }
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

/// Try to resolve the image from a local engine.
/// Args:
///  - `engine`: `docker` or `podman`
///  - `image_ref`: Configured image reference
///  - `pinned`: Pinned digest of the reference, if any.
///
/// Returns:
///   The resolved image. `Ok(None)` if the image is not there (or the engine
///   is not installed). `Err(reason)` if the image is there but not usable.
///   The caller decides if that is fatal or a reason to try the next source.
fn resolve_local(
    engine: &'static str,
    image_ref: &str,
    pinned: Option<&str>,
) -> Result<Option<ResolvedImage>, String> {
    // `docker images` matches on repo:tag and does not know the `@sha256:…`
    // suffix. Query without it and check the pin separately.
    let query_ref = match pinned {
        Some(_) => image_ref
            .rsplit_once('@')
            .map_or(image_ref, |(name, _)| name),
        None => image_ref,
    };
    let Some(image_id) = docker::image_exists(engine, query_ref) else {
        return Ok(None);
    };

    // A local tag can point to a different image than the same tag in the
    // registry. Check a pinned digest against the digests that the engine
    // recorded at pull time. Do not trust the name.
    if let Some(want) = pinned
        && !docker::repo_digests(engine, &image_id)
            .iter()
            .any(|d| d == want)
    {
        return Err(format!(
            "{engine} image {query_ref} does not match the pinned digest {want}"
        ));
    }

    let host_arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    let image_arch = docker::image_arch(engine, &image_id).unwrap_or_default();
    if !image_arch.is_empty() && image_arch != host_arch {
        return Err(format!("{engine} image is {image_arch}, need {host_arch}"));
    }

    cli::log!(
        "  {} image resolved via {engine} {}",
        cli::check(),
        cli::dim(&image_id[..19.min(image_id.len())])
    );
    Ok(Some(ResolvedImage {
        digest: image_id,
        config: OciConfig::default(),
        source: ImageSource::Local {
            engine,
            image_ref: query_ref.to_string(),
        },
    }))
}

/// An image resolved to a concrete digest, ready for download.
///
/// Invariant: `digest` names the image that `source` gives. Both come from
/// a single [`resolve_image`] call. Do not combine them with other values.
/// With a different digest, [`ensure_image`] writes the content of one
/// image to the cache entry of a different image.
struct ResolvedImage {
    digest: String,
    config: OciConfig,
    source: ImageSource,
}

/// Where the content of a resolved image comes from.
enum ImageSource {
    Local {
        engine: &'static str,
        image_ref: String,
    },
    Registry(Box<registry::RegistryImage>),
}

/// Make sure that all layers of the image are in the layer cache, and store
/// the image metadata at `images/<digest>`.
///
/// The host has no merged rootfs. The guest makes the overlayfs directly
/// from the per-layer cache. Registry and docker images both use
/// [`layer::ensure_layer_cached`].
/// Args:
///  - `resolved`: The resolved image. Local resolution sets its config here
///  - `image_name`: Configured image reference
///  - `auth`: Registry auth
///  - `insecure`: Allow plain-HTTP registry access.
///
/// Returns:
///   The cached image metadata, or error.
async fn ensure_image(
    resolved: &mut ResolvedImage,
    image_name: &str,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<OciImage> {
    let image_path = crate::cache::image_path(&resolved.digest)?;

    // Digest-keyed cache hit: a sibling project already pulled this image
    // and all its layers are still on disk. Skip the pull. Write the current
    // name, so the per-sandbox fast path in `prepare()` (which matches on
    // name) sees the current tag.
    if let Some(mut cached) = read_ready_image(&image_path).filter(|c| c.user.is_some()) {
        if cached.name != image_name {
            cached.name = image_name.to_string();
            write_cached_image(&image_path, &cached)?;
        }
        return Ok(cached);
    }

    let ordered_layers = match &resolved.source {
        ImageSource::Local { engine, image_ref } => {
            let image_ref = image_ref.clone();
            let (cfg, layers) = ensure_local_image(engine, &image_ref).await?;
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

/// Export an image from a local engine and extract its layers into the
/// shared per-layer cache. Ctrl+C stops the export.
/// Args:
///  - `engine`: `docker` or `podman`
///  - `image_ref`: Image reference without a digest pin.
///
/// Returns:
///   The parsed image config and the layer keys, topmost first.
async fn ensure_local_image(
    engine: &'static str,
    image_ref: &str,
) -> anyhow::Result<(OciConfig, Vec<String>)> {
    let sp = cli::spinner(&format!("exporting from {engine}..."));

    let image_ref = image_ref.to_string();
    let pipeline = async {
        let save = docker::save_layer_tarballs(engine, &image_ref).await?;
        for digest in &save.layer_digests {
            let digest = digest.clone();
            tokio::task::spawn_blocking(move || {
                layer::ensure_layer_cached(
                    &digest,
                    |_tmp| {
                        anyhow::bail!(
                            "{engine} save stream did not include blob for layer {digest} \
                             (manifest referenced a layer that was not in the export)"
                        )
                    },
                    None,
                )
            })
            .await??;
        }
        Ok::<_, anyhow::Error>(save)
    };

    // The full pipeline (save and per-layer extract) runs against
    // `cli::interrupted`. On Ctrl+C, the drop guard of the save side kills
    // the docker child, and the extract loop stops at the current layer.
    // The next GC sweep removes any partial `.tmp/` extraction.
    let save = tokio::select! {
        res = pipeline => res?,
        () = cli::interrupted() => {
            sp.finish_and_clear();
            anyhow::bail!("cancelled by user");
        }
    };

    sp.finish_and_clear();
    cli::log!("  {} exported from {engine}", cli::check());

    // Docker save manifests list layers bottom first. Overlayfs needs
    // topmost first.
    let mut ordered: Vec<String> = save
        .layer_digests
        .iter()
        .map(|d| cache::layer_key(d))
        .collect();
    ordered.reverse();
    Ok((save.image_config, ordered))
}

/// Pull the layers of a registry image and extract them into the shared
/// per-layer cache. Ctrl+C stops the pull.
/// Args:
///  - `reg`: The resolved registry image
///  - `auth`: Registry auth
///  - `insecure`: Allow plain-HTTP registry access.
///
/// Returns:
///   The layer keys, topmost first.
async fn ensure_registry_image(
    reg: &registry::RegistryImage,
    auth: &RegistryAuth,
    insecure: bool,
) -> anyhow::Result<Vec<String>> {
    let layers = &reg.manifest.layers;

    let cached_count = layers
        .iter()
        .filter(|l| cache::layer_dir(&cache::layer_key(&l.digest)).is_ok_and(|p| p.is_dir()))
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
        .filter(|(_, l)| !cache::layer_dir(&cache::layer_key(&l.digest)).is_ok_and(|p| p.is_dir()))
        .map(|(i, _)| i)
        .collect();

    if !to_fetch.is_empty() {
        let mp = cli::multi_progress();
        let reference = &reg.reference;

        // One bar for each layer. Cached layers start full, so the display
        // shows progress for the full image, not only for the downloads.
        let fetch_set: std::collections::HashSet<usize> = to_fetch.iter().copied().collect();
        let bars: Vec<indicatif::ProgressBar> = layers
            .iter()
            .enumerate()
            .map(|(i, layer_desc)| {
                let pb = cli::layer_progress_bar(&mp, layer_desc.size as u64);
                if !fetch_set.contains(&i) {
                    pb.set_position(layer_desc.size as u64);
                    pb.set_message("cached");
                }
                pb
            })
            .collect();
        let _spacer = cli::progress_spacer(&mp);
        let bars_ref = &bars;

        // Download at most 3 layers at the same time.
        let fetch = async {
            let mut stream = stream::iter(to_fetch.iter().copied())
                .map(|i| async move {
                    let layer_desc = &layers[i];
                    let per_layer = &bars_ref[i];
                    fetch_and_extract_layer(reference, layer_desc, per_layer, auth, insecure).await
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
                anyhow::bail!("cancelled by user");
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

    // OCI manifests list layers bottom first. Overlayfs needs topmost first.
    let mut ordered: Vec<String> = layers.iter().map(|l| cache::layer_key(&l.digest)).collect();
    ordered.reverse();
    Ok(ordered)
}

/// Download one layer blob and extract it into the shared per-layer cache.
///
/// [`layer::ensure_layer_cached`] does nothing if the layer dir already
/// exists. Thus the `to_fetch` filter in the caller only makes it faster.
/// Correct operation does not need it.
/// Args:
///  - `reference`: Image reference in the registry
///  - `layer_desc`: Descriptor of the layer
///  - `per_layer`: Progress bar of the layer
///  - `auth`: Registry auth
///  - `insecure`: Allow plain-HTTP registry access.
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

    // `ensure_layer_cached` does blocking I/O (tar extraction), so it must
    // not run on the async runtime. Thus pull the blob with async code into
    // a temp file, then run the extraction in a blocking task.
    let layers_root = cache::layers_root()?;
    let key = cache::layer_key(&digest);
    let download = layers_root.join(format!("{key}.download"));

    // Same fast path as in `ensure_layer_cached`.
    let layer_dir = cache::layer_dir(&key)?;
    if layer_dir.is_dir() {
        return Ok(());
    }

    if !download.exists() {
        let download_tmp = layer::download_tmp_path(&layers_root, &key);
        let pulled = registry::pull_layer(
            &reference,
            &layer_desc,
            &download_tmp,
            Some(&per_layer),
            None,
            &auth,
            insecure,
        )
        .await
        .and_then(|()| Ok(std::fs::rename(&download_tmp, &download)?));
        if pulled.is_err() {
            // The name is unique, so no later pull reuses or removes it.
            let _ = std::fs::remove_file(&download_tmp);
        }
        pulled?;
    }

    tokio::task::spawn_blocking(move || {
        layer::ensure_layer_cached(
            &digest,
            |_tmp| {
                // The async pull above made `.download`, so this fetch
                // closure does not run. If it runs, fail with an error.
                anyhow::bail!("unreachable: layer tarball missing after pull")
            },
            Some(&per_layer),
        )
    })
    .await??;
    Ok(())
}

/// Format a byte count for humans (`B`, `KB` or `MB`).
fn format_size(bytes: i64) -> String {
    if bytes < 1024 {
        format!("{bytes}B")
    } else if bytes < 1024 * 1024 {
        format!("{:.1}KB", bytes as f64 / 1024.0)
    } else {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    }
}

/// Find the first `:`-separated record in a file of the image layers.
///
/// Reads the individual layer trees under `<data>/oci/layers/`,
/// because the host has no merged rootfs. This is less exact than real
/// overlayfs:
///  * A whiteout is an empty file. It has no matches, so the search
///    continues in the next layer. This is safe for the usual images that
///    never delete `/etc/passwd` in an upper layer.
///  * If an upper layer *removed* a record from its copy of the file, the
///    search still finds the record in a lower layer. The search continues
///    past a file that exists but has no match. A merged rootfs stops there.
///
/// Args:
///  - `layer_keys`: Layer keys, topmost first
///  - `rel_path`: File path in the layer (e.g. `etc/passwd`)
///  - `pick`: Gets the fields of a line, and returns a value on a match.
///
/// Returns:
///   The first match, and all layer copies of the file that were refused
///   instead of read, with the reason.
fn lookup_layer_record<T>(
    layer_keys: &[String],
    rel_path: &str,
    pick: impl Fn(&[&str]) -> Option<T>,
) -> anyhow::Result<Lookup<T>> {
    let mut ignored = Vec::new();
    for key in layer_keys {
        let content = match read_layer_file(&cache::layer_dir(key)?, rel_path) {
            Ok(Some(content)) => content,
            Ok(None) => continue,
            Err(Refused { why, suspicious }) => {
                if suspicious {
                    tracing::warn!("{rel_path} in layer {key}: {why}, ignoring");
                } else {
                    tracing::debug!("{rel_path} in layer {key}: {why}, ignoring");
                }
                ignored.push(format!("{rel_path} in layer {key}: {why}"));
                continue;
            }
        };
        for line in content.lines() {
            let fields: Vec<&str> = line.split(':').collect();
            if let Some(value) = pick(&fields) {
                return Ok(Lookup {
                    value: Some(value),
                    ignored,
                });
            }
        }
    }
    Ok(Lookup {
        value: None,
        ignored,
    })
}

/// Result of [`lookup_layer_record`]: the first match, and the layer copies
/// that were refused instead of read. The refusals are important only if
/// nothing matched. Then they show the difference between "this image is
/// broken" and "airlock rejected this image", so they go into the error.
struct Lookup<T> {
    value: Option<T>,
    ignored: Vec<String>,
}

impl<T> Lookup<T> {
    /// Like `Option::ok_or_else`, but the error also lists the refused
    /// layer files, so the user knows *why* nothing resolved.
    fn ok_or_else(self, not_found: impl FnOnce() -> String) -> anyhow::Result<T> {
        if let Some(value) = self.value {
            return Ok(value);
        }
        let msg = if self.ignored.is_empty() {
            not_found()
        } else {
            format!("{} (ignored: {})", not_found(), self.ignored.join("; "))
        };
        Err(anyhow::anyhow!(msg))
    }
}

/// Read a file from one layer tree. Refuse a read that goes outside that
/// tree.
///
/// Tar extraction keeps the symlinks of a layer as they are, because they
/// must resolve inside the *guest*. But here they resolve on the host. Thus
/// an image can contain `etc/passwd -> /etc/passwd` (read the host users),
/// `-> /dev/zero` (read until OOM) or `etc -> /` (both). Symlinks that stay
/// in the layer (`etc/passwd -> ../usr/lib/passwd`) are correct and still
/// resolve.
/// Args:
///  - `layer_dir`: Root directory of the layer tree
///  - `rel_path`: File path in the layer.
///
/// Returns:
///   The file content. `Ok(None)` if the layer has no such file. `Err` if
///   the read was refused. Callers treat a refusal as "no records in this
///   layer" and continue to the next layer, the same as for a whiteout. But
///   they can report the refusal if nothing else resolves.
fn read_layer_file(layer_dir: &Path, rel_path: &str) -> Result<Option<String>, Refused> {
    use std::io::Read;

    let root = std::fs::canonicalize(layer_dir)
        .map_err(|e| Refused::suspicious(format!("cannot resolve layer dir: {e}")))?;

    // Examine one component at a time. `lstat` does not follow the last
    // component, but it follows symlinks in the parent components. Thus a
    // single check of `etc/passwd` follows `etc -> /` to the host. Each
    // symlink on the path gets the same stay-inside check.
    let mut path = layer_dir.to_path_buf();
    for component in rel_path.split('/') {
        path.push(component);
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            return Ok(None);
        };
        if meta.file_type().is_symlink() {
            let target = std::fs::canonicalize(&path).map_err(|e| {
                Refused::unresolved(format!(
                    "symlink target cannot be resolved in this layer: {e}"
                ))
            })?;
            if !target.starts_with(&root) {
                return Err(Refused::suspicious(format!(
                    "symlink resolves outside the layer ({})",
                    target.display()
                )));
            }
        }
    }

    let meta = std::fs::metadata(&path).map_err(|e| Refused::suspicious(e.to_string()))?;
    if !meta.is_file() {
        return Err(Refused::suspicious("not a regular file".to_string()));
    }
    if meta.len() > MAX_LAYER_RECORD_FILE {
        return Err(Refused::suspicious(format!(
            "{} bytes, larger than the {MAX_LAYER_RECORD_FILE} byte limit",
            meta.len()
        )));
    }
    let mut content = String::new();
    std::fs::File::open(&path)
        .map_err(|e| Refused::suspicious(e.to_string()))?
        .take(MAX_LAYER_RECORD_FILE)
        .read_to_string(&mut content)
        .map_err(|e| Refused::suspicious(e.to_string()))?;
    Ok(Some(content))
}

/// Why a layer file was not read.
///
/// `suspicious` is `true` for things that an honest image never does: a
/// symlink out of the layer, a directory or device instead of a file, or a
/// file that is too large. It is `false` for a symlink whose target is in a
/// different layer. That is a limit of per-layer reads, and a warning on
/// each prepare is not useful.
struct Refused {
    why: String,
    suspicious: bool,
}

impl Refused {
    /// A refusal that an honest image never causes.
    fn suspicious(why: String) -> Self {
        Self {
            why,
            suspicious: true,
        }
    }

    /// A symlink whose target cannot be resolved in this layer.
    fn unresolved(why: String) -> Self {
        Self {
            why,
            suspicious: false,
        }
    }
}

/// Read a file from the image layers, topmost layer first.
///
/// Uses the same containment rules as the user lookup
/// ([`read_layer_file`]). It skips a symlink out of its layer and a file
/// that is too large. An empty file (a whiteout in the layer cache) also
/// continues to the next layer.
/// Args:
///  - `layer_keys`: Layer keys, topmost first
///  - `rel_path`: File path in the image (e.g. `etc/os-release`).
///
/// Returns:
///   The file content, or `None` if no layer has the file.
pub fn read_image_file(layer_keys: &[String], rel_path: &str) -> Option<String> {
    let dirs: Vec<_> = layer_keys
        .iter()
        .filter_map(|key| cache::layer_dir(key).ok())
        .collect();
    read_file_in_layers(&dirs, rel_path)
}

/// [`read_image_file`] over explicit layer directories, topmost first.
fn read_file_in_layers(layer_dirs: &[std::path::PathBuf], rel_path: &str) -> Option<String> {
    for dir in layer_dirs {
        match read_layer_file(dir, rel_path) {
            Ok(Some(content)) if !content.is_empty() => return Some(content),
            Ok(_) => {}
            Err(Refused { why, .. }) => {
                tracing::debug!("{rel_path} in {}: {why}, ignoring", dir.display());
            }
        }
    }
    None
}

/// The distribution identity of an image, from `os-release`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OsRelease {
    /// `ID`, e.g. `alpine`, `debian`, `ubuntu`.
    pub id: String,
    /// `ID_LIKE`, split on whitespace, e.g. `["debian"]` for Ubuntu.
    pub id_like: Vec<String>,
}

/// Read `etc/os-release` (or `usr/lib/os-release`, its usual link target)
/// from the image layers.
/// Returns:
///   The distribution identity, or `None` if neither file exists or has no
///   `ID`.
pub fn os_release(image: &OciImage) -> Option<OsRelease> {
    ["etc/os-release", "usr/lib/os-release"]
        .iter()
        .find_map(|rel| read_image_file(&image.image_layers, rel))
        .and_then(|content| parse_os_release(&content))
}

/// Parse the `ID` and `ID_LIKE` keys of an os-release file (shell-style
/// `KEY=value`, quotes optional).
fn parse_os_release(content: &str) -> Option<OsRelease> {
    let mut id = None;
    let mut id_like = Vec::new();
    for line in content.lines() {
        let Some((key, value)) = line.trim().split_once('=') else {
            continue;
        };
        let value = value.trim().trim_matches(|c| c == '"' || c == '\'');
        match key.trim() {
            "ID" => id = Some(value.to_ascii_lowercase()),
            "ID_LIKE" => {
                id_like = value
                    .split_whitespace()
                    .map(str::to_ascii_lowercase)
                    .collect();
            }
            _ => {}
        }
    }
    id.filter(|id| !id.is_empty())
        .map(|id| OsRelease { id, id_like })
}

/// Look up a user's home directory by uid in the image's `/etc/passwd`.
fn lookup_home_dir(layer_keys: &[String], uid: u32) -> anyhow::Result<String> {
    lookup_layer_record(layer_keys, "etc/passwd", |f| {
        (f.len() >= 6 && f[2].parse::<u32>().ok() == Some(uid)).then(|| f[5].to_string())
    })?
    .ok_or_else(|| format!("no home directory found for uid {uid} in any layer /etc/passwd"))
}

#[cfg(test)]
mod tests;
