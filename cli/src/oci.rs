pub(crate) mod config;
mod docker;
mod layer;
mod registry;

use std::path::{Path, PathBuf};

use oci_client::config::ConfigFile as OciConfig;

use crate::cli;
use crate::cli::CliArgs;
use crate::project::Project;
use crate::terminal::Terminal;

pub struct Bundle {
    pub path: PathBuf,
    /// Mounts with `~` expanded: source to host home, target to container home.
    pub mounts: Vec<ResolvedMount>,
}

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

pub enum MountType {
    Dir { key: String },
    File { filename: String },
}

impl ResolvedMount {
    /// Mount key to VM /mnt
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
            MountType::File { filename } => format!("/mnt/{}/{filename}", self.key()),
        }
    }
}

/// Resolve, download, and prepare the OCI bundle for the project.
pub async fn prepare(
    args: &CliArgs,
    project: &Project,
    terminal: &Terminal,
) -> anyhow::Result<Bundle> {
    let mut image = resolve_image(&project.config.image).await?;
    let image_dir = ensure_image(&mut image).await?;
    let bundle_path = ensure_rootfs(&image_dir, &image.digest, project)?;

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
    mounts.push(install_ca_cert(&image_dir, &bundle_path, &project.ca_cert)?);

    // Resolve mount paths: expand ~ on source (host home) and target (container home)
    let container_uid = config::get_uid(&image.config);
    let container_home = lookup_home_dir(&bundle_path.join("rootfs"), container_uid)?;
    let host_home = dirs::home_dir().unwrap_or_default();
    mounts.extend(resolve_mounts(
        &project.config.mounts,
        &host_home,
        &container_home,
    )?);

    // Write OCI config.json now that all binds are assembled
    config::generate_config(
        &image.config,
        &project.cwd,
        &mounts,
        &args.args,
        terminal.is_tty(),
        &bundle_path.join("config.json"),
    )?;

    Ok(Bundle {
        path: bundle_path,
        mounts,
    })
}

async fn resolve_image(image_ref: &str) -> anyhow::Result<ResolvedImage> {
    cli::log!(
        "Preparing project environment using image {}...",
        cli::dim(image_ref)
    );

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

    if rootfs.exists() {
        if matches!(resolved.source, ImageSource::Docker { .. }) {
            let config_path = dir.join("image_config.json");
            if config_path.exists() {
                let data = std::fs::read(&config_path)?;
                resolved.config = serde_json::from_slice(&data)?;
            }
        }
        return Ok(dir);
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
        }
    }

    Ok(dir)
}

/// Ensure the project bundle rootfs exists (CoW copy of image).
///
/// The digest file is the consistency marker — it's written last after
/// a successful rootfs copy. If it's missing or stale, the bundle is
/// considered incomplete and gets recreated.
fn ensure_rootfs(
    image_dir: &Path,
    image_digest: &str,
    project: &Project,
) -> anyhow::Result<PathBuf> {
    let bundle = project.cache_dir.join("bundle");
    let digest_file = project.cache_dir.join("image_digest");
    let rootfs = bundle.join("rootfs");

    // Check digest: missing = incomplete, mismatched = image changed
    let stored_digest = std::fs::read_to_string(&digest_file).ok();
    let digest_matches = stored_digest
        .as_deref()
        .is_some_and(|s| s.trim() == image_digest);

    if stored_digest.is_some() && !digest_matches {
        // Image changed — prompt in interactive mode
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

        match choice {
            0 => {
                let spinner = cli::spinner("erasing old environment...");
                let _ = std::fs::remove_dir_all(&bundle);
                let _ = std::fs::remove_file(&digest_file);
                spinner.finish_and_clear();
                cli::log!("  {} old environment erased", cli::check());
            }
            1 => {
                cli::log!("  {} environment ready", cli::check());
                return Ok(bundle);
            }
            _ => anyhow::bail!("cancelled by user"),
        }
    }

    if !digest_matches {
        // Incomplete or missing bundle — clean up any partial state and recreate
        let _ = std::fs::remove_dir_all(&rootfs);
        let _ = std::fs::remove_file(&digest_file);

        let spinner = cli::spinner("creating project environment...");
        std::fs::create_dir_all(&bundle)?;
        crate::cache::cow_copy(&image_dir.join("rootfs"), &rootfs)?;
        // Write digest last — marks the bundle as complete
        std::fs::write(&digest_file, image_digest)?;
        spinner.finish_and_clear();
    }

    cli::log!("  {} environment ready", cli::check());
    Ok(bundle)
}

/// Rewrite the container's trust store with the image's original certs
/// plus the project CA cert for TLS MITM.
fn install_ca_cert(
    image_dir: &Path,
    bundle_path: &Path,
    ca_cert_path: &Path,
) -> anyhow::Result<ResolvedMount> {
    let ca_store = "rootfs/etc/ssl/certs/ca-certificates.crt";
    let dest = bundle_path.join(ca_store);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read(image_dir.join(ca_store)).unwrap_or_default();
    let ca_cert = std::fs::read(ca_cert_path)?;
    let mut out = existing;
    if !out.ends_with(b"\n") && !out.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(&ca_cert);
    std::fs::write(&dest, &out)?;
    Ok(ResolvedMount {
        display: None,
        mount_type: MountType::File {
            filename: ca_cert_path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into(),
        },
        source: ca_cert_path.to_path_buf(),
        target: "/etc/ssl/certs/ca-certificates.crt".to_string(),
        read_only: true,
    })
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
        // passwd format: name:password:uid:gid:gecos:home:shell
        if fields.len() >= 6 && fields[2].parse::<u32>().ok() == Some(uid) {
            return Ok(fields[5].to_string());
        }
    }

    anyhow::bail!("no home directory found for uid {uid} in container /etc/passwd")
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

/// Resolve mount paths: expand ~ on source (host home) and target (container home).
/// Determines mount type (dir with unique tag, or file) based on source path.
fn resolve_mounts(
    mounts: &[crate::config::config::Mount],
    host_home: &Path,
    container_home: &str,
) -> anyhow::Result<Vec<ResolvedMount>> {
    let container_home = PathBuf::from(container_home);
    mounts
        .iter()
        .enumerate()
        .map(|(i, m)| {
            let source = expand_tilde(&m.source, host_home);
            let source = std::fs::canonicalize(&source).unwrap_or(source);
            let target = expand_tilde(&m.target, &container_home);

            let mount_type = if source.is_dir() {
                MountType::Dir {
                    key: format!("mount_{i}"),
                }
            } else {
                MountType::File {
                    filename: source
                        .file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .into(),
                }
            };

            Ok(ResolvedMount {
                display: Some((m.source.clone(), m.target.clone())),
                mount_type,
                source,
                target: target.to_string_lossy().to_string(),
                read_only: m.read_only,
            })
        })
        .collect()
}
