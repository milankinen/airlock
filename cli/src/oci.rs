pub(crate) mod cache;
pub(crate) mod config;
mod docker;
mod layer;
mod registry;
#[cfg(test)]
mod tests;

use std::path::{Path, PathBuf};

use oci_client::config::ConfigFile as OciConfig;

use crate::cli;
use crate::cli::CliArgs;
use crate::project::Project;
use crate::terminal::Terminal;

pub struct Bundle {
    /// Mounts with `~` expanded: source to host home, target to container home.
    pub mounts: Vec<ResolvedMount>,
    /// Sparse disk image for overlay upper + cache (always present).
    pub disk_image: PathBuf,
    /// Path to the shared read-only image rootfs in the image cache.
    pub image_rootfs: PathBuf,
}

#[derive(Debug)]
pub struct ResolvedMount {
    /// Source + target (with `~`) for display (only for config mounts)
    pub display: Option<(String, String)>,
    /// Mount type: file / directory
    pub mount_type: MountType,
    /// Expanded absolute source path on host.
    pub source: PathBuf,
    /// Expanded absolute target path in container.
    pub target: String,
    pub read_only: bool,
}

#[derive(Debug)]
pub enum MountType {
    Dir { key: String },
    File { filename: String },
}

impl ResolvedMount {
    pub fn key(&self) -> &str {
        match &self.mount_type {
            MountType::Dir { key } => key.as_str(),
            MountType::File { filename: _ } if self.read_only => "files_ro",
            MountType::File { filename: _ } => "files_rw",
        }
    }
    pub fn vm_path(&self) -> String {
        match &self.mount_type {
            MountType::Dir { key } => format!("/mnt/{key}"),
            MountType::File { filename } => format!("overlay/{}/{filename}", self.key()),
        }
    }
}

/// Resolve, download, and prepare the OCI bundle for the project.
pub async fn prepare(
    args: &CliArgs,
    project: &Project,
    terminal: &Terminal,
) -> anyhow::Result<Bundle> {
    let digest_file = project.cache_dir.join("image_digest");
    let stored_digest = std::fs::read_to_string(&digest_file).ok();

    // Resolve image reference to a digest (no download yet)
    cli::log!(
        "Preparing project environment using image {}...",
        cli::dim(&project.config.image)
    );
    let mut image = resolve_image(&project.config.image).await?;

    // Check if image changed before downloading
    let mut digest_changed = stored_digest
        .as_deref()
        .is_none_or(|s| s.trim() != image.digest);

    if let Some(old_digest) = stored_digest
        && digest_changed
    {
        match prompt_image_changed()? {
            ImageChangeAction::KeepOld => {
                digest_changed = false;
            }
            ImageChangeAction::Recreate => {
                let spinner = cli::spinner("erasing old environment...");
                let _ = std::fs::remove_dir_all(project.cache_dir.join("overlay"));
                let _ = std::fs::remove_file(&digest_file);
                spinner.finish_and_clear();
                cli::log!("  {} old environment erased", cli::check());
                // GC: check if old image is still used by any project
                gc_unused_image(old_digest.trim())?;
            }
            ImageChangeAction::Cancel => anyhow::bail!("cancelled by user"),
        }
    }

    // Download/ensure image
    let image_dir = ensure_image(&mut image).await?;

    if digest_changed {
        std::fs::write(&digest_file, &image.digest)?;
    }

    let overlay_dir = project.cache_dir.join("overlay");
    std::fs::create_dir_all(&overlay_dir)?;
    install_ca_cert(&image_dir, &overlay_dir, &project.ca_cert)?;
    cli::log!("  {} environment ready", cli::check());

    build_bundle(
        args,
        project,
        terminal,
        &overlay_dir,
        &image_dir,
        &image.digest,
    )
}

fn build_bundle(
    args: &CliArgs,
    project: &Project,
    terminal: &Terminal,
    overlay_dir: &Path,
    image_dir: &Path,
    image_id: &str,
) -> anyhow::Result<Bundle> {
    let mut mounts = vec![];
    mounts.push(ResolvedMount {
        display: None,
        mount_type: MountType::Dir {
            key: "project".to_string(),
        },
        source: project.cwd.clone(),
        target: project.cwd.to_string_lossy().into(),
        read_only: false,
    });

    // Read image config from the image cache
    let config_path = image_dir.join("image_config.json");
    let image_config: OciConfig = if let Ok(data) = std::fs::read(&config_path) {
        serde_json::from_slice(&data).unwrap_or_default()
    } else {
        OciConfig::default()
    };

    let container_uid = config::get_uid(&image_config);
    let image_rootfs = image_dir.join("rootfs");
    let container_home = lookup_home_dir(&image_rootfs, container_uid)?;
    let host_home = dirs::home_dir().unwrap_or_default();
    let enabled_mounts: Vec<_> = project
        .config
        .mounts
        .values()
        .filter(|m| m.enabled)
        .cloned()
        .collect::<Vec<_>>();
    mounts.extend(resolve_mounts(
        &enabled_mounts,
        &host_home,
        &container_home,
        &project.cwd,
    )?);

    // Disk image (ext4) for overlay upper + cache mounts
    let (disk_image, cache_targets) = cache::prepare(
        &project.cache_dir,
        &project.config.disk,
        &container_home,
        &project.cwd,
    )?;

    let pty_size = if terminal.is_tty() {
        // crossterm::terminal::size() returns (cols, rows)
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        Some((rows, cols))
    } else {
        None
    };
    config::generate_config(
        &image_config,
        &project.cwd,
        &mounts,
        &args.args,
        pty_size,
        &overlay_dir.join("config.json"),
    )?;

    // Write mounts.json for the supervisor to read
    let dirs_json: Vec<serde_json::Value> = mounts
        .iter()
        .filter(|m| matches!(m.mount_type, MountType::Dir { .. }))
        .map(|m| {
            serde_json::json!({
                "tag": m.key(),
                "target": m.target,
                "read_only": m.read_only,
            })
        })
        .collect();
    let files_json: Vec<serde_json::Value> = mounts
        .iter()
        .filter(|m| matches!(m.mount_type, MountType::File { .. }))
        .map(|m| {
            serde_json::json!({
                "target": m.target,
                "read_only": m.read_only,
            })
        })
        .collect();
    let mounts_json = serde_json::json!({
        "image_id": image_id,
        "dirs": dirs_json,
        "files": files_json,
        "cache": cache_targets,
    });
    std::fs::write(
        overlay_dir.join("mounts.json"),
        serde_json::to_string_pretty(&mounts_json)?,
    )?;

    Ok(Bundle {
        mounts,
        disk_image,
        image_rootfs,
    })
}

enum ImageChangeAction {
    Recreate,
    KeepOld,
    Cancel,
}

fn prompt_image_changed() -> anyhow::Result<ImageChangeAction> {
    if !cli::is_interactive() {
        anyhow::bail!("project image has changed");
    }
    let term = dialoguer::console::Term::stderr();
    let choice = dialoguer::Select::new()
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
async fn resolve_image(image_ref: &str) -> anyhow::Result<ResolvedImage> {
    if let Some(image_id) = docker::image_exists(image_ref) {
        cli::log!(
            "  {} resolved via docker {}",
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

    let reg = registry::resolve(image_ref).await?;
    cli::log!(
        "  {} resolved {}",
        cli::check(),
        cli::dim(&format!("{}@{}", reg.reference, &reg.digest[..19]))
    );
    Ok(ResolvedImage {
        digest: reg.digest.clone(),
        config: reg.image_config.clone(),
        source: ImageSource::Registry(Box::new(reg)),
    })
}

struct ResolvedImage {
    digest: String,
    config: OciConfig,
    source: ImageSource,
}

enum ImageSource {
    Docker { image_ref: String },
    Registry(Box<registry::RegistryImage>),
}

async fn ensure_image(resolved: &mut ResolvedImage) -> anyhow::Result<PathBuf> {
    let dir = crate::cache::image_dir(&resolved.digest)?;
    let rootfs = dir.join("rootfs");
    let complete_marker = dir.join(".complete");

    if rootfs.exists() && complete_marker.exists() {
        if matches!(resolved.source, ImageSource::Docker { .. }) {
            let config_path = dir.join("image_config.json");
            if config_path.exists() {
                let data = std::fs::read(&config_path)?;
                resolved.config = serde_json::from_slice(&data)?;
            }
        }
        return Ok(dir);
    }

    // Incomplete or corrupt image — clean up and re-extract
    if rootfs.exists() {
        tracing::debug!("image extraction incomplete, cleaning up");
        let _ = std::fs::remove_dir_all(&rootfs);
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
            std::fs::write(&complete_marker, "")?;
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
                        result = registry::pull_layer(&reg.reference, layer_desc, &layer_path, Some(&pb)) => {
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
            std::fs::write(&complete_marker, "")?;
        }
    }

    Ok(dir)
}

/// Install the project CA cert into overlay/files_rw so the container
/// sees the combined (image + project) CA trust stores via overlayfs.
/// Different distros use different paths — write to all common locations.
fn install_ca_cert(
    image_dir: &Path,
    overlay_dir: &Path,
    ca_cert_path: &Path,
) -> anyhow::Result<()> {
    let ca_cert = std::fs::read(ca_cert_path)?;

    // Paths relative to rootfs for CA trust stores across distros
    let ca_stores = [
        "etc/ssl/certs/ca-certificates.crt", // Debian/Ubuntu
        "etc/ssl/cert.pem",                  // Alpine/LibreSSL
        "etc/pki/tls/certs/ca-bundle.crt",   // RHEL/CentOS
        "etc/ssl/ca-bundle.pem",             // openSUSE
    ];

    for ca_store in ca_stores {
        let dest = overlay_dir.join("files_rw").join(ca_store);
        // Read pristine certs from image (may not exist for all paths)
        let existing = std::fs::read(image_dir.join("rootfs").join(ca_store)).unwrap_or_default();
        let mut out = existing;
        if !out.ends_with(b"\n") && !out.is_empty() {
            out.push(b'\n');
        }
        out.extend_from_slice(&ca_cert);
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&dest, &out)?;
    }

    Ok(())
}

/// Check if an image digest is used by any project. If not, delete it.
fn gc_unused_image(digest: &str) -> anyhow::Result<()> {
    let projects_dir = crate::cache::cache_dir()?.join("projects");
    if !projects_dir.exists() {
        return Ok(());
    }

    for entry in std::fs::read_dir(&projects_dir)?.flatten() {
        let digest_file = entry.path().join("image_digest");
        if let Ok(stored) = std::fs::read_to_string(&digest_file)
            && stored.trim() == digest
        {
            // Still in use by another project
            return Ok(());
        }
    }

    // No project references this image — delete it
    let image_dir = crate::cache::image_dir(digest)?;
    if image_dir.exists() {
        let sp = cli::spinner("cleaning unused image...");
        let _ = std::fs::remove_dir_all(&image_dir);
        sp.finish_and_clear();
        cli::log!("  {} cleaned unused image", cli::check());
    }

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

/// Expand `~` prefix in a path string.
fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
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

pub(crate) fn resolve_mounts(
    mounts: &[crate::config::config::Mount],
    host_home: &Path,
    container_home: &str,
    cwd: &Path,
) -> anyhow::Result<Vec<ResolvedMount>> {
    use crate::config::config::MissingAction;

    let container_home = PathBuf::from(container_home);
    let mut result = Vec::new();

    for (i, m) in mounts.iter().enumerate() {
        let source = expand_tilde(&m.source, host_home);
        // Resolve relative paths against cwd
        let source = if source.is_relative() {
            cwd.join(&source)
        } else {
            source
        };

        // Handle missing source
        if !source.exists() {
            match m.missing {
                MissingAction::Fail => {
                    anyhow::bail!("mount source does not exist: {}", source.display());
                }
                MissingAction::Warn => {
                    crate::cli::log!(
                        "  {} mount skipped (not found): {}",
                        crate::cli::bullet(),
                        crate::cli::dim(&source.display().to_string())
                    );
                    continue;
                }
                MissingAction::Ignore => continue,
                MissingAction::Create => {
                    std::fs::create_dir_all(&source)?;
                }
            }
        }

        let source = std::fs::canonicalize(&source).unwrap_or(source);
        let target = expand_tilde(&m.target, &container_home);

        let file_name = source
            .file_name()
            .map_or_else(|| format!("file_{i}"), |n| n.to_string_lossy().to_string());

        let mount_type = if source.is_dir() {
            MountType::Dir {
                key: format!("mount_{i}"),
            }
        } else {
            MountType::File {
                filename: file_name,
            }
        };

        result.push(ResolvedMount {
            display: Some((m.source.clone(), m.target.clone())),
            source,
            mount_type,
            target: target.to_string_lossy().to_string(),
            read_only: m.read_only,
        });
    }

    Ok(result)
}
