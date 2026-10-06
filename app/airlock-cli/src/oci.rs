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

/// Largest `etc/passwd` / `etc/group` accepted from a layer. Real files are
/// a few KB; anything bigger is treated as having no records rather than
/// read whole, so an image cannot dictate how much memory `prepare` uses.
const MAX_LAYER_RECORD_FILE: u64 = 1024 * 1024;

/// Everything needed to configure the container process (returned by `prepare`).
/// Mount resolution, disk setup, and env overrides happen in `vm::start` (env
/// via the resolved `SandboxEnv`); command overrides in `sandbox::main_argv`.
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
    /// Ordered layer keys — topmost-first. Each entry is a versioned layer
    /// name ([`cache::layer_key`]), matching both the on-disk directory name
    /// under `~/.cache/airlock/oci/layers/<key>` and the guest mount path
    /// `/mnt/layers/<key>`.
    pub image_layers: Vec<String>,
    /// Container home directory derived from the image's user record
    /// (e.g. `/root`). For guest-path `~` expansion the
    /// [`effective_container_home`] helper should be preferred — the
    /// user can override `HOME` from `[env]`, in which case tilde
    /// expansion needs to track the override or we'd resolve to a
    /// path that doesn't match what `$HOME` actually points to inside
    /// the sandbox.
    pub container_home: String,
    /// Container uid (from image config).
    pub uid: u32,
    /// Container gid (from image config).
    pub gid: u32,
    /// Raw image entrypoint+cmd merged, `/bin/sh` fallback if empty.
    /// No command overrides (those go in `sandbox::main_argv`).
    pub cmd: Vec<String>,
    /// Base defaults (PATH/TERM/HOME) + image env.
    /// No `[env]` overrides (those are layered in `vm::start` from `SandboxEnv`).
    pub env: Vec<String>,
    /// The raw `USER` string from the image config (`""` when the image
    /// declares none). `None` marks a file written before named users were
    /// resolved through the image's `/etc/passwd`
    /// (<https://github.com/milankinen/airlock/pull/12>): such a file may
    /// carry root `uid`/`gid` for an image that asked for a non-root user, so
    /// [`prepare`] re-verifies it against a fresh resolution before use.
    #[serde(default)]
    pub user: Option<String>,
}

/// What [`prepare`] does when the configured image resolves to a digest
/// other than the one the sandbox was created with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnImageChange {
    /// Ask the user: re-create the sandbox, continue with the current one,
    /// or cancel.
    Ask,
    /// Re-create the sandbox without asking (`--yes`).
    Recreate,
    /// Stop with [`ImageChangeStop::NeedsTerminal`]: nobody can answer.
    Refuse,
}

/// What [`prepare`] did about the image of the sandbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageChange {
    /// The image is the one the sandbox was created with (or the sandbox
    /// had none yet).
    Unchanged,
    /// The image changed and the sandbox must be re-created: the caller
    /// deletes the sandbox disk and the install records.
    Recreate,
    /// The image changed, and the sandbox continues with its old image.
    KeepOld,
    /// The image changed and the sandbox was to continue with its old
    /// image, but that is not available any more (not complete in the
    /// cache): the sandbox continues with the new image.
    OldImageGone,
}

/// Why [`prepare`] stopped early (a typed error inside the
/// `anyhow::Error`).
#[derive(Debug, thiserror::Error)]
pub enum ImageChangeStop {
    /// No terminal to ask on, and no `--yes`.
    #[error("Sandbox image has been changed. Run in a terminal or pass --yes.")]
    NeedsTerminal,
    /// The user chose Cancel (or pressed Esc).
    #[error("cancelled by user")]
    Cancelled,
    /// Ctrl+C while the question was open or the image downloaded.
    #[error("interrupted")]
    Interrupted,
}

/// The prepared image of a sandbox, and what happened to the old one.
pub struct PreparedImage {
    pub image: OciImage,
    pub change: ImageChange,
}

/// Resolve, download, and prepare the OCI image `image_cfg` for the sandbox
/// at `sandbox_dir`. `vault` holds the registry credentials.
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

    // The cached image this sandbox is currently running, if it is both
    // complete on disk and still the image the config asks for by name.
    let cached = read_ready_image(&sandbox_image).filter(|img| img.name == *image_name);

    // A file without `user` predates named-USER resolution and may carry a
    // wrong uid/gid — it has to be re-verified below, so it never takes the
    // fast path and never serves as a digest-keyed cache hit.
    let legacy = cached.as_ref().is_some_and(|img| img.user.is_none());

    // Fast path: reuse the cached image and skip the network round-trip that
    // resolves tag → digest.
    //
    // `if-changed` gives up that shortcut on purpose — spending the
    // round-trip is the whole point of the policy. A digest-pinned reference
    // is exempt either way: it names one immutable image, so a matching name
    // already implies a matching digest and there is nothing to detect.
    if let Some(img) = cached.clone()
        && !legacy
        && (image_cfg.pull_policy == PullPolicy::IfNotPresent
            || image_cfg.pinned_digest().is_some())
    {
        tracing::debug!("image cache hit for {image_name}");
        return use_cached_image(sandbox_dir, &sandbox_image, img).map(unchanged);
    }

    // Fall-through: read just the stored digest (if any) for change detection.
    let stored_digest = read_cached_image(&sandbox_image).map(|i| i.image_id);

    // Set up registry auth: use stored credentials, fall back to anonymous.
    let registry_host: String = image_name
        .parse::<oci_client::Reference>()
        .map_or_else(|_| image_name.clone(), |r| r.resolve_registry().to_string());

    let (mut image, auth) = match resolve_with_auth(vault, image_cfg, &registry_host).await {
        Ok(resolved) => resolved,
        Err(e) => {
            // Under `if-changed` the source was contacted only to ask whether
            // a newer digest exists. A registry that is down, unreachable, or
            // mid-outage shouldn't strand a sandbox whose image is already
            // sitting complete on disk — offer to carry on with it.
            let Some(img) = cached.filter(|_| !cli::is_interrupted()) else {
                return Err(e);
            };
            if !prompt_resolution_failed(&e)? {
                return Err(e);
            }
            return use_cached_image(sandbox_dir, &sandbox_image, img).map(unchanged);
        }
    };

    // Same digest, but the cached metadata was baked by the old `USER`
    // resolution: re-derive uid/gid from the fresh config and either stamp
    // the file as verified or, if the sandbox has been running as the wrong
    // user, refuse to start it.
    if let Some(img) = cached
        .as_ref()
        .filter(|i| i.user.is_none() && i.image_id == image.digest)
    {
        verify_legacy_user(img, &image)?;
    }

    // Check if image changed before downloading.
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
            // "Still intact" has to mean *ready*, not merely present: the
            // JSON can outlive its layer trees, which a sweep collects
            // independently. Checking only for the file and then handing
            // the old digest to `ensure_image` would miss that, fall
            // through to the pull path, and persist the **new** image's
            // layers and config under the **old** digest — poisoning that
            // cache entry for every sandbox that shares it, and reporting
            // an `image_id` the supervisor uses for change detection that
            // describes neither image.
            //
            // Returning here instead of rewriting `image.digest` keeps
            // that mismatch unrepresentable rather than merely unlikely:
            // no digest can reach `ensure_image` unless it came from the
            // same resolution as the source beside it.
            let old_image_path = crate::cache::image_path(old_digest.trim())?;
            if let Some(mut old) = read_ready_image(&old_image_path) {
                // Stamp the configured name onto the kept image so the
                // name-keyed fast path recognizes it on the next start —
                // otherwise every subsequent run re-resolves and asks this
                // same question again.
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
            // Remove image ref hard link — drops this sandbox's liveness signal
            // for the old image, so the sweep below may collect it.
            let _ = std::fs::remove_file(&sandbox_image);
            cli::log!("  {} old environment erased", cli::check());
            // GC: remove images with no remaining sandbox refs, plus any
            // layers they uniquely owned.
            gc::sweep();
        }
    }

    // Download/ensure image (auth already resolved above).
    let oci_image = tokio::select! {
        res = ensure_image(&mut image, image_name, &auth, image_cfg.insecure) => res?,
        () = cli::interrupted() => return Err(ImageChangeStop::Interrupted.into()),
    };
    // Hard-link the cached image file into the sandbox directory. nlink > 1
    // on `images/<digest>` is the GC guard — without it, a sibling sandbox
    // creating a new image could trigger a sweep that wrongly deletes this
    // one. Enforce unconditionally: even when the digest hasn't changed, a
    // previous run may have left the sandbox with a standalone copy instead
    // of a hardlink.
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

/// Finish `prepare` with an image already cached on disk: re-establish the GC
/// hardlink, make sure the overlay dir exists, and report it as ready.
///
/// Shared by the fast path and the resolution-failure fallback so both leave
/// the sandbox in exactly the same state as a freshly pulled image would.
fn use_cached_image(
    sandbox_dir: &Path,
    sandbox_image: &Path,
    image: OciImage,
) -> anyhow::Result<OciImage> {
    // Invariant: `sandbox/image` must be a hardlink to the canonical cache
    // file so sweep GC sees the sandbox as a live reference. Heal it on every
    // prepare — the link may have been severed by a cache wipe, a cache-path
    // migration, or a prior run that pre-dated this invariant, leaving our
    // entry as a sweep target.
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

/// Resolve the configured reference to a digest, negotiating registry auth.
///
/// Starts anonymous, falls back to vault-stored credentials, and finally
/// prompts — retrying until resolution succeeds or the user interrupts.
/// Returns the auth that worked so the caller can reuse it for the pull.
async fn resolve_with_auth(
    vault: &Vault,
    image_cfg: &ImageRef,
    registry_host: &str,
) -> anyhow::Result<(ResolvedImage, RegistryAuth)> {
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

/// On-disk wrapper for a cached [`OciImage`]. Internally tagged so the JSON
/// carries `"schema":"v2"` alongside the image fields. The schema version
/// is bumped in lockstep with [`crate::cache::LAYER_FORMAT`] so a layer
/// format change makes every old image JSON fail to deserialize and force
/// a clean re-pull.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(tag = "schema")]
enum CachedImage {
    #[serde(rename = "v2")]
    V2(OciImage),
}

/// Read a cached image JSON file and unwrap it into an [`OciImage`]. Returns
/// `None` when the file is absent, unreadable, or written by an
/// unrecognized schema version — callers treat any of those as a cache miss.
fn read_cached_image(path: &Path) -> Option<OciImage> {
    let data = std::fs::read(path).ok()?;
    let wrapped: CachedImage = serde_json::from_slice(&data).ok()?;
    let CachedImage::V2(image) = wrapped;
    Some(image)
}

/// Like [`read_cached_image`], but also requires every referenced layer to
/// still exist on disk. Returns `None` for both "no such file" and "file
/// there but some layer was swept" — both mean the caller must re-resolve.
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
    let bytes = serde_json::to_vec_pretty(&CachedImage::V2(image.clone()))?;
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
    let user = cfg.and_then(|c| c.user.as_deref()).unwrap_or("");
    let (uid, gid) = resolve_user(&ordered_layers, user)?;
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

/// Resolve the home directory that `~` should expand to for paths the
/// sandbox sees. Falls back to the OCI image's user record when the
/// project doesn't set `HOME` in `[env]`; otherwise honours the user's
/// override as the guest will see it (already `${VAR}`-substituted by
/// [`crate::project::SandboxEnv::resolve`] in `project::open` /
/// [`crate::project::Project::with_config`]).
///
/// Without this, a `target = "~/foo"` mount with `[env].HOME = "/x"`
/// would expand the `~` against the image's home (`/root`) but the
/// sandbox shell would resolve `$HOME` as `/x` — paths land in the
/// wrong place and tools that re-tilde a result of the mount mismatch
/// what's actually mounted.
pub fn effective_container_home(project: &Project, image: &OciImage) -> String {
    project
        .env
        .guest_value("HOME")
        .map_or_else(|| image.container_home.clone(), str::to_string)
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

/// Resolve an image `USER` string into numeric uid/gid.
///
/// The OCI image spec allows `user`, `uid`, `user:group`, `uid:gid`,
/// `uid:group` and `user:gid`. Names are looked up in the image's own
/// `/etc/passwd` and `/etc/group` (via [`lookup_layer_record`]), matching
/// what Docker does; a bare user with no group part takes that user's
/// primary gid from `passwd`. An empty string means root, as it does for
/// an image that never sets `USER`.
///
/// A name that no layer declares is an error rather than a fallback to
/// root: silently promoting an image that asked for an unprivileged user
/// is exactly the outcome the image author wrote `USER` to prevent.
fn resolve_user(layer_keys: &[String], user: &str) -> anyhow::Result<(u32, u32)> {
    let (user_part, group_part) = match user.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (user, None),
    };

    // Each passwd record is `name:pw:uid:gid:gecos:home:shell`; a match
    // yields `(uid, primary gid)`.
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

/// Resolve the group half of a `USER` string: a numeric gid is used as-is,
/// a name is looked up in the image's `/etc/group`.
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

/// Ask what to do about the changed image of an existing sandbox. Esc and
/// Cancel stop with [`ImageChangeStop::Cancelled`].
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

/// Ask whether to fall back to the cached image after resolution failed.
/// Returns `true` to continue with the cache, `false` to abort (the
/// default, Esc, or no terminal).
///
/// Non-interactive runs abort: a resolution failure is a genuine error, and
/// silently substituting a possibly stale image in CI would hide it.
fn prompt_resolution_failed(err: &anyhow::Error) -> anyhow::Result<bool> {
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

/// Outcome of re-checking a legacy cache file's uid/gid against a fresh
/// resolution of the image's `USER`.
#[derive(Debug, PartialEq, Eq)]
enum LegacyUser {
    /// The stored uid/gid are what the fixed resolution produces.
    Verified,
    /// Same uid, different primary group (`USER 1000` used to get gid 0).
    /// Existing files still belong to the same owner, so the cache entry is
    /// repaired in place instead of throwing the sandbox away.
    GidOnly { gid: u32 },
    /// The sandbox has been running as the wrong user — typically root for
    /// `USER node` — and its disk holds state owned by that user.
    UidMismatch { uid: u32, gid: u32 },
}

/// Re-derive uid/gid for a legacy cache file (one without `user`) from the
/// current `USER` string, using the layers the file already references.
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

/// The `USER` string of a freshly resolved image. Registry resolution
/// already carries the config; local resolution defers it to the export,
/// so ask the engine directly.
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

/// `stored` is this sandbox's cached image, written before named `USER`
/// resolution existed and still naming the digest `resolved` just produced.
/// Stamp it verified (or repair its gid) when the fixed resolution agrees;
/// otherwise fail with an explanation and point at `airlock rm` — after the
/// removal the next start finds no sandbox image, and `ensure_image` rebuilds
/// the shared entry rather than reusing the legacy one.
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
    // Rewrite the shared cache entry; `prepare` re-links the sandbox copy to
    // the new file via `ensure_image_hardlink`.
    write_cached_image(&crate::cache::image_path(&stored.image_id)?, &fixed)
}

/// Full image resolution (with config).
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
            // Present locally but unusable. With a single engine there is
            // nowhere else to look, so surface why rather than the generic
            // not-found.
            Err(reason) if local_only => anyhow::bail!("{reason}"),
            Err(reason) => cli::log!("  {} {reason} — trying next", cli::bullet()),
            Ok(None) => {}
        }
    }
    if local_only {
        anyhow::bail!("image {image_ref} not found in {}", engines[0]);
    }

    let reg = registry::resolve(image_ref, auth, image_cfg.insecure).await?;
    // A pin can name either the multi-platform index or the platform manifest
    // selected from it; both are legitimate things to copy out of a registry.
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

/// Try to satisfy the reference from the local `engine` (docker or podman).
///
/// `Ok(None)` means the image simply isn't there (or the engine isn't
/// installed); `Err(reason)` means it is, but can't be used — the caller
/// decides whether that is fatal or just a reason to try the next source.
fn resolve_local(
    engine: &'static str,
    image_ref: &str,
    pinned: Option<&str>,
) -> Result<Option<ResolvedImage>, String> {
    // `docker images` matches on repo:tag and knows nothing about the
    // `@sha256:…` suffix, so query without it and verify the pin separately.
    let query_ref = match pinned {
        Some(_) => image_ref
            .rsplit_once('@')
            .map_or(image_ref, |(name, _)| name),
        None => image_ref,
    };
    let Some(image_id) = docker::image_exists(engine, query_ref) else {
        return Ok(None);
    };

    // A local tag can point somewhere else entirely than the same tag in the
    // registry, so a pinned digest must be checked against what the engine
    // recorded when it pulled the image — not assumed from the name.
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

/// An image resolved to a concrete digest, ready to be downloaded.
///
/// Invariant: `digest` names the image that `source` will produce. Both come
/// from a single [`resolve_image`] call and must not be recombined — swapping
/// in some other digest makes [`ensure_image`] write one image's content to
/// another image's cache entry.
struct ResolvedImage {
    digest: String,
    config: OciConfig,
    source: ImageSource,
}

enum ImageSource {
    Local {
        engine: &'static str,
        image_ref: String,
    },
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

/// Stream `docker image save` and extract each referenced layer through the
/// shared per-layer cache. Returns the parsed image config plus layer
/// digests in topmost-first order.
///
/// The whole pipeline (save + per-layer extract) races against
/// [`cli::interrupted`]; on Ctrl+C the docker child is killed via the
/// save-side drop guard and the extract loop stops at the current layer.
/// Any partial `.tmp/` extraction is left behind for the next sweep GC.
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

    let save = tokio::select! {
        res = pipeline => res?,
        () = cli::interrupted() => {
            sp.finish_and_clear();
            anyhow::bail!("cancelled by user");
        }
    };

    sp.finish_and_clear();
    cli::log!("  {} exported from {engine}", cli::check());

    // Docker save manifests are bottom-up; overlayfs wants topmost first.
    let mut ordered: Vec<String> = save
        .layer_digests
        .iter()
        .map(|d| cache::layer_key(d))
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

        // Bar per layer — cached layers get pre-filled so the display shows
        // progress for the whole image, not just the slice we're downloading.
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

    // OCI manifests list layers bottom→top; overlayfs wants topmost first.
    let mut ordered: Vec<String> = layers.iter().map(|l| cache::layer_key(&l.digest)).collect();
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
    let layers_root = cache::layers_root()?;
    let key = cache::layer_key(&digest);
    let download = layers_root.join(format!("{key}.download"));
    let download_tmp = layers_root.join(format!("{key}.download.tmp"));

    // Short-circuit fast path identical to ensure_layer_cached.
    let layer_dir = cache::layer_dir(&key)?;
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
        layer::ensure_layer_cached(
            &digest,
            |_tmp| {
                // .download already exists from the async pull above, so the
                // fetch closure is not called. If somehow it is, fail loudly.
                anyhow::bail!("unreachable: layer tarball missing after pull")
            },
            Some(&per_layer),
        )
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

/// Walk `rel_path` (e.g. `etc/passwd`) through the per-layer cache, topmost
/// first, splitting each line on `:` and returning the first record for
/// which `pick` yields a value — together with every layer copy that was
/// refused rather than read, and why.
///
/// Reads from individual layer trees under `~/.cache/airlock/oci/layers/` —
/// the host has no merged rootfs to consult for this lookup. Whiteouts
/// manifest as empty files (which parse to zero matches and fall through
/// to the next layer); this is coarser than real overlayfs semantics but
/// is a safe superset for the common case of images that never delete
/// `/etc/passwd` in an upper layer. The same coarseness means a record an
/// upper layer *removed* from its copy of the file is still found in a
/// lower layer's copy — the walk keeps going past a file that exists but
/// has no match, where a merged rootfs would have stopped at it.
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

/// Outcome of [`lookup_layer_record`]: the first match, plus the layer
/// copies that were refused instead of read. The refusals only matter when
/// nothing matched — then they are the difference between "this image is
/// broken" and "this image was rejected", so they go into the error.
struct Lookup<T> {
    value: Option<T>,
    ignored: Vec<String>,
}

impl<T> Lookup<T> {
    /// Like `Option::ok_or_else`, but the error also lists the refused
    /// layer files so the user learns *why* nothing resolved.
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

/// Read `rel_path` from one layer tree, refusing anything that would take
/// the read outside that tree.
///
/// Tar extraction keeps a layer's symlinks verbatim because they are meant
/// to resolve inside the *guest* — but here they resolve on the host. An
/// image can therefore ship `etc/passwd -> /etc/passwd` (read the host's
/// users), `-> /dev/zero` (read until OOM) or `etc -> /` (both). Symlinks
/// that stay within the layer (`etc/passwd -> ../usr/lib/passwd`) are
/// legitimate and still resolve. Anything refused reads as "no records in
/// this layer", so the walk falls through to the next layer the same way it
/// does for a whiteout — but unlike a whiteout the refusal is returned as
/// `Err`, so it can be reported if nothing else resolves. `Ok(None)` means
/// the layer simply has no such file.
fn read_layer_file(layer_dir: &Path, rel_path: &str) -> Result<Option<String>, Refused> {
    use std::io::Read;

    let root = std::fs::canonicalize(layer_dir)
        .map_err(|e| Refused::suspicious(format!("cannot resolve layer dir: {e}")))?;

    // Walk one component at a time. `lstat` refuses to follow only the
    // *last* component, so checking `etc/passwd` in one go would traverse
    // `etc -> /` and, if the host happens to lack `/passwd`, report the
    // layer as merely not having the file. Every symlink on the way gets
    // the same stay-inside test as the final one.
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

/// Why a layer file was not read. `suspicious` separates what an honest
/// image never does (a symlink escaping the layer, a directory or device
/// where a file should be, an oversized file) from a symlink whose target
/// lives in another layer, which is a limit of reading per-layer trees and
/// not worth a warning every prepare.
struct Refused {
    why: String,
    suspicious: bool,
}

impl Refused {
    fn suspicious(why: String) -> Self {
        Self {
            why,
            suspicious: true,
        }
    }

    fn unresolved(why: String) -> Self {
        Self {
            why,
            suspicious: false,
        }
    }
}

/// Read `rel_path` (e.g. `etc/os-release`) from the image's layers,
/// topmost layer first, with the same containment rules as the user
/// lookup ([`read_layer_file`]): a symlink out of its layer or an oversized
/// file is skipped. An empty file (how a whiteout looks in the layer
/// cache) falls through to the next layer. `None` when no layer has it.
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

/// Read `etc/os-release` (or `usr/lib/os-release`, which it usually links
/// to) from the image layers. `None` when neither exists or has no `ID`.
pub fn os_release(image: &OciImage) -> Option<OsRelease> {
    ["etc/os-release", "usr/lib/os-release"]
        .iter()
        .find_map(|rel| read_image_file(&image.image_layers, rel))
        .and_then(|content| parse_os_release(&content))
}

/// Parse the `ID` and `ID_LIKE` keys of an os-release file (shell-style
/// `KEY=value`, optionally quoted).
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
mod tests {
    use super::*;
    use crate::cache::HOME_LOCK;

    fn tempfile_dir() -> std::path::PathBuf {
        let base = std::env::temp_dir().join(format!(
            "airlock-oci-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn sample_image(digest: &str, layers: &[&str]) -> OciImage {
        OciImage {
            image_id: digest.to_string(),
            name: "alpine:3.19".to_string(),
            image_layers: layers.iter().map(|d| cache::layer_key(d)).collect(),
            container_home: "/root".to_string(),
            uid: 0,
            gid: 0,
            cmd: vec!["/bin/sh".to_string()],
            env: vec![],
            user: Some(String::new()),
        }
    }

    fn make_layer(digest: &str) {
        let dir = cache::layer_dir(&cache::layer_key(digest)).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker"), b"x").unwrap();
    }

    /// The "keep old environment" branch decides whether the old image is
    /// reusable with `read_ready_image`, not with a plain existence check.
    /// The distinction is the whole fix: an image JSON outlives its layer
    /// trees (a sweep collects them independently), and treating that
    /// half-collected state as reusable used to send the old digest down the
    /// pull path, persisting the *new* image's layers under it.
    #[test]
    fn cached_image_missing_layers_is_not_ready() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let path = cache::image_path("sha256:swept").unwrap();
        write_cached_image(&path, &sample_image("sha256:swept", &["sha256:L1"])).unwrap();

        // The metadata is on disk and readable...
        assert!(path.exists(), "image JSON should exist");
        assert!(read_cached_image(&path).is_some());
        // ...but its layer tree was swept, so it is not reusable.
        assert!(
            read_ready_image(&path).is_none(),
            "an image whose layers were swept must not count as ready"
        );

        // Restoring the layer makes the very same entry reusable again.
        make_layer("sha256:L1");
        assert!(read_ready_image(&path).is_some());
    }

    /// A layerless entry can't be composed into a rootfs, so it is never
    /// "ready" no matter what else is on disk.
    #[test]
    fn cached_image_without_layers_is_not_ready() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let path = cache::image_path("sha256:empty").unwrap();
        write_cached_image(&path, &sample_image("sha256:empty", &[])).unwrap();

        assert!(read_ready_image(&path).is_none());
    }

    /// Write a layer whose `/etc/passwd` and `/etc/group` declare root and
    /// one unprivileged user, the way every `node`, `python`, `debian`
    /// derived image does.
    fn make_layer_with_passwd(digest: &str) -> String {
        let key = cache::layer_key(digest);
        let dir = cache::layer_dir(&key).unwrap();
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::fs::write(
            dir.join("etc/passwd"),
            "root:x:0:0:root:/root:/bin/sh\nnode:x:1000:1000:Node:/home/node:/bin/sh\n",
        )
        .unwrap();
        std::fs::write(dir.join("etc/group"), "root:x:0:\nnode:x:1000:\n").unwrap();
        key
    }

    /// Point a layer's `etc/passwd` (or its whole `etc` directory) at a
    /// path outside the layer tree — where a malicious image can aim a
    /// symlink at the host's `/etc/passwd`, `/dev/zero`, or a FIFO.
    fn make_layer_with_passwd_symlink(digest: &str, link: &Path, dir_link: bool) -> String {
        use std::os::unix::fs::symlink;
        let key = cache::layer_key(digest);
        let dir = cache::layer_dir(&key).unwrap();
        if dir_link {
            std::fs::create_dir_all(&dir).unwrap();
            symlink(link, dir.join("etc")).unwrap();
        } else {
            std::fs::create_dir_all(dir.join("etc")).unwrap();
            symlink(link, dir.join("etc/passwd")).unwrap();
        }
        key
    }

    /// A layer's `etc/passwd` symlinked to a file *outside* the layer must
    /// never be read: that file is the host's, not the image's. The record
    /// has to come from a lower layer's real file — or not at all.
    #[test]
    fn passwd_symlink_outside_layer_is_not_followed() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        // Stand-in for a host file: same uid as the image's `node`, but a
        // home directory only the outside file knows about.
        let outside = tmp.join("host-passwd");
        std::fs::write(
            &outside,
            "node:x:1000:1000:Host:/leaked-from-host:/bin/sh\n",
        )
        .unwrap();
        let outside_dir = tmp.join("host-etc");
        std::fs::create_dir_all(&outside_dir).unwrap();
        std::fs::write(outside_dir.join("passwd"), std::fs::read(&outside).unwrap()).unwrap();

        let real = make_layer_with_passwd("sha256:real-passwd");

        // Upper layer symlinks the file itself.
        let file_link = make_layer_with_passwd_symlink("sha256:file-link", &outside, false);
        assert_eq!(
            lookup_home_dir(&[file_link.clone(), real.clone()], 1000).unwrap(),
            "/home/node",
            "a symlinked etc/passwd must be skipped in favour of the lower layer's real file"
        );
        assert!(
            lookup_home_dir(&[file_link], 1000).is_err(),
            "with no real passwd in any layer the lookup must fail, not read the host file"
        );

        // Upper layer symlinks the whole `etc` directory.
        let dir_link = make_layer_with_passwd_symlink("sha256:dir-link", &outside_dir, true);
        assert_eq!(
            lookup_home_dir(&[dir_link.clone(), real], 1000).unwrap(),
            "/home/node",
            "a symlinked etc/ directory must be skipped too"
        );
        assert!(lookup_home_dir(&[dir_link], 1000).is_err());
    }

    /// When nothing resolves *and* a layer's file was refused, the error
    /// has to say so. Otherwise a hostile image that pointed `passwd` at
    /// the host and a merely broken image produce the same "no user found",
    /// and the one line that tells them apart sits in a log nobody opens.
    #[test]
    fn refused_passwd_is_named_in_the_error() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let outside = tmp.join("host-passwd");
        std::fs::write(&outside, "node:x:1000:1000:Host:/leaked:/bin/sh\n").unwrap();
        let link = make_layer_with_passwd_symlink("sha256:named-link", &outside, false);

        let err = lookup_home_dir(std::slice::from_ref(&link), 1000)
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no home directory found for uid 1000"),
            "keeps the original not-found text: {err}"
        );
        assert!(
            err.contains("etc/passwd") && err.contains("outside the layer"),
            "names the refused file and why: {err}"
        );

        let err = resolve_user(std::slice::from_ref(&link), "node")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("no user node found") && err.contains("outside the layer"),
            "named-USER resolution reports the refusal too: {err}"
        );

        // An oversized file is refused for a different reason, and the
        // error should say that one instead.
        let key = cache::layer_key("sha256:named-huge");
        let dir = cache::layer_dir(&key).unwrap();
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        let mut huge = "#".repeat(MAX_LAYER_RECORD_FILE as usize + 1);
        huge.push_str("\nnode:x:1000:1000:Node:/home/node:/bin/sh\n");
        std::fs::write(dir.join("etc/passwd"), huge).unwrap();
        let err = lookup_home_dir(&[key], 1000).unwrap_err().to_string();
        assert!(err.contains("larger than"), "names the size refusal: {err}");
    }

    /// `lstat` refuses to follow only the *last* path component. A layer
    /// with `etc -> <outside>` where `<outside>` happens to lack a `passwd`
    /// must still be reported as an escaping symlink, not read as "this
    /// layer has no such file".
    #[test]
    fn escaping_directory_symlink_is_reported_even_without_target_file() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let empty_outside = tmp.join("host-etc-empty");
        std::fs::create_dir_all(&empty_outside).unwrap();
        let link = make_layer_with_passwd_symlink("sha256:dir-link-empty", &empty_outside, true);

        let err = lookup_home_dir(&[link], 1000).unwrap_err().to_string();
        assert!(
            err.contains("outside the layer"),
            "escaping `etc` must be named even though `etc/passwd` does not exist: {err}"
        );
    }

    /// The two remaining refusal reasons, each from an input tar can
    /// actually produce: a *directory* named `etc/passwd` (not a regular
    /// file) and a symlink whose target is missing from this layer.
    #[test]
    fn directory_and_dangling_symlink_refusals_are_named() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let dir_key = cache::layer_key("sha256:passwd-is-a-dir");
        std::fs::create_dir_all(cache::layer_dir(&dir_key).unwrap().join("etc/passwd")).unwrap();
        let err = lookup_home_dir(&[dir_key], 1000).unwrap_err().to_string();
        assert!(err.contains("not a regular file"), "{err}");

        let dangling_key = cache::layer_key("sha256:passwd-dangling");
        let dir = cache::layer_dir(&dangling_key).unwrap();
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::os::unix::fs::symlink("../usr/lib/passwd", dir.join("etc/passwd")).unwrap();
        let err = lookup_home_dir(&[dangling_key], 1000)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot be resolved"), "{err}");
    }

    /// Symlinks that stay *inside* the layer are legitimate (merged-usr
    /// style `etc/passwd -> ../usr/lib/passwd`) and must keep resolving.
    #[test]
    fn passwd_symlink_inside_layer_is_followed() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let key = cache::layer_key("sha256:internal-link");
        let dir = cache::layer_dir(&key).unwrap();
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::fs::create_dir_all(dir.join("usr/lib")).unwrap();
        std::fs::write(
            dir.join("usr/lib/passwd"),
            "node:x:1000:1000:Node:/home/node:/bin/sh\n",
        )
        .unwrap();
        std::os::unix::fs::symlink("../usr/lib/passwd", dir.join("etc/passwd")).unwrap();

        assert_eq!(lookup_home_dir(&[key], 1000).unwrap(), "/home/node");
    }

    /// `/etc/passwd` is a few kilobytes. A layer whose copy is enormous is
    /// not a passwd file, and reading it whole would let an image dictate
    /// how much host memory `prepare` allocates.
    #[test]
    fn oversized_passwd_is_not_read() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let key = cache::layer_key("sha256:huge-passwd");
        let dir = cache::layer_dir(&key).unwrap();
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        // Filler comment lines past the cap, then a record the old code
        // would happily find at the end.
        let mut huge = "#".repeat(MAX_LAYER_RECORD_FILE as usize + 1);
        huge.push_str("\nnode:x:1000:1000:Node:/home/node:/bin/sh\n");
        std::fs::write(dir.join("etc/passwd"), huge).unwrap();

        assert!(
            lookup_home_dir(&[key], 1000).is_err(),
            "an oversized passwd must be treated as having no records"
        );
    }

    /// Cache files written before `user` existed must still load — as
    /// legacy entries with `user == None` — so `prepare` can verify them
    /// instead of rejecting them or trusting them blindly.
    #[test]
    fn cache_file_without_user_loads_as_legacy() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }

        let path = cache::image_path("sha256:legacy").unwrap();
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let legacy = serde_json::json!({
            "schema": "v2",
            "image_id": "sha256:legacy",
            "name": "node:22",
            "image_layers": [cache::layer_key("sha256:L1")],
            "container_home": "/root",
            "uid": 0,
            "gid": 0,
            "cmd": ["/bin/sh"],
            "env": [],
        });
        std::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        assert_eq!(read_cached_image(&path).unwrap().user, None);

        write_cached_image(&path, &sample_image("sha256:legacy", &["sha256:L1"])).unwrap();
        assert_eq!(read_cached_image(&path).unwrap().user, Some(String::new()));
    }

    /// A legacy entry baked by the fallback-to-root resolution is only a
    /// problem when the uid it stored differs from what `USER` really means.
    /// A wrong primary group alone is repairable; a wrong uid is not.
    #[test]
    fn legacy_user_check_distinguishes_root_fallback_from_gid_drift() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let layer = make_layer_with_passwd("sha256:legacy-passwd");
        let mut stored = sample_image("sha256:legacy", &[]);
        stored.image_layers = vec![layer];
        stored.user = None;

        // Root image, or one that never declared a user: nothing to fix.
        assert_eq!(
            check_legacy_user(&stored, "").unwrap(),
            LegacyUser::Verified
        );
        assert_eq!(
            check_legacy_user(&stored, "root").unwrap(),
            LegacyUser::Verified
        );
        // `USER node` fell back to root before the fix.
        assert_eq!(
            check_legacy_user(&stored, "node").unwrap(),
            LegacyUser::UidMismatch {
                uid: 1000,
                gid: 1000
            }
        );

        // `USER 1000` got the right uid but gid 0 instead of the primary group.
        stored.uid = 1000;
        assert_eq!(
            check_legacy_user(&stored, "1000").unwrap(),
            LegacyUser::GidOnly { gid: 1000 }
        );
        stored.gid = 1000;
        assert_eq!(
            check_legacy_user(&stored, "1000").unwrap(),
            LegacyUser::Verified
        );
    }

    fn config_with_user(user: &str) -> OciConfig {
        OciConfig {
            config: Some(oci_client::config::Config {
                user: Some(user.to_string()),
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    /// The OCI image spec allows `USER` to be `user`, `uid`, `user:group`,
    /// `uid:gid`, `uid:group` or `user:gid`. Docker resolves names through the
    /// image's `/etc/passwd`; the manual (technical/container-execution.md)
    /// says airlock does the same. An image that says `USER node` must
    /// therefore run as uid 1000, not as root.
    #[test]
    fn named_user_in_image_config_does_not_become_root() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let layer = make_layer_with_passwd("sha256:passwd-layer");

        for user in [
            "node",
            "1000",
            "node:node",
            "1000:node",
            "node:1000",
            "1000:1000",
        ] {
            let image = build_oci_image(
                "sha256:img".to_string(),
                "node:22".to_string(),
                vec![layer.clone()],
                &config_with_user(user),
            )
            .unwrap();

            assert_eq!(
                (image.uid, image.gid),
                (1000, 1000),
                "USER {user:?} must resolve through /etc/passwd, not fall back to root"
            );
            assert_eq!(
                image.container_home, "/home/node",
                "USER {user:?} must get the named user's home"
            );
        }
    }

    /// A `USER` name the image never declares is the same mistake as a
    /// typo in a Dockerfile, and Docker refuses to start such an image.
    /// Falling back to root here would quietly hand out the privilege the
    /// image author was trying to give up.
    #[test]
    fn unknown_user_or_group_name_is_an_error_not_root() {
        let _guard = HOME_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let tmp = tempfile_dir();
        unsafe {
            std::env::set_var("HOME", &tmp);
        }
        let layer = make_layer_with_passwd("sha256:passwd-layer-unknown");

        for user in ["ghost", "ghost:node", "node:ghost", "1000:ghost"] {
            let result = build_oci_image(
                "sha256:img".to_string(),
                "node:22".to_string(),
                vec![layer.clone()],
                &config_with_user(user),
            );
            let err = result
                .err()
                .unwrap_or_else(|| panic!("USER {user:?} must be rejected, not resolved"));
            assert!(
                err.to_string().contains("ghost"),
                "error for USER {user:?} should name the missing entry: {err}"
            );
        }
    }

    /// Topmost layer wins; an empty (whiteout) file and a symlink out of
    /// the layer fall through to the next layer.
    #[test]
    fn read_file_in_layers_prefers_the_top_and_skips_whiteouts_and_escapes() {
        let base = tempfile_dir();
        let top = base.join("top");
        let mid = base.join("mid");
        let bottom = base.join("bottom");
        for d in [&top, &mid, &bottom] {
            std::fs::create_dir_all(d.join("etc")).unwrap();
        }
        std::fs::write(top.join("etc/os-release"), "").unwrap();
        std::os::unix::fs::symlink("/etc/hostname", mid.join("etc/os-release")).unwrap();
        std::fs::write(bottom.join("etc/os-release"), "ID=debian\n").unwrap();
        let layers = vec![top.clone(), mid, bottom];
        assert_eq!(
            read_file_in_layers(&layers, "etc/os-release").as_deref(),
            Some("ID=debian\n")
        );
        std::fs::write(top.join("etc/os-release"), "ID=alpine\n").unwrap();
        assert_eq!(
            read_file_in_layers(&layers, "etc/os-release").as_deref(),
            Some("ID=alpine\n")
        );
        assert!(read_file_in_layers(&layers, "etc/missing").is_none());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Alpine links `etc/os-release` to `../usr/lib/os-release`; a link
    /// inside the layer resolves.
    #[test]
    fn read_file_in_layers_follows_links_inside_the_layer() {
        let base = tempfile_dir();
        std::fs::create_dir_all(base.join("etc")).unwrap();
        std::fs::create_dir_all(base.join("usr/lib")).unwrap();
        std::fs::write(base.join("usr/lib/os-release"), "ID=alpine\n").unwrap();
        std::os::unix::fs::symlink("../usr/lib/os-release", base.join("etc/os-release")).unwrap();
        assert_eq!(
            read_file_in_layers(std::slice::from_ref(&base), "etc/os-release").as_deref(),
            Some("ID=alpine\n")
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_os_release_reads_id_and_id_like() {
        let ubuntu = parse_os_release("NAME=\"Ubuntu\"\nID=ubuntu\nID_LIKE=debian\n").unwrap();
        assert_eq!(ubuntu.id, "ubuntu");
        assert_eq!(ubuntu.id_like, vec!["debian".to_string()]);
        let rocky = parse_os_release("ID=\"rocky\"\nID_LIKE=\"rhel centos fedora\"").unwrap();
        assert_eq!(rocky.id_like.len(), 3);
        assert!(parse_os_release("NAME=x\n").is_none());
    }
}
