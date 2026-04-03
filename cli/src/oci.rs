pub mod cache;
pub(crate) mod config;
mod docker;
mod layer;
mod registry;

use std::path::{Path, PathBuf};

use crate::cli::Cli;
use crate::project::Project;
use crate::terminal::Terminal;
use crate::vm::Vm;
use crate::vm::mounts::ContainerBind;

pub struct Bundle {
    pub path: PathBuf,
}

/// Resolve, download, and prepare the OCI bundle for the project.
pub async fn prepare(
    cli: &Cli,
    project: &Project,
    terminal: &Terminal,
    vm: &Vm,
) -> anyhow::Result<Bundle> {
    let mut resolved = resolve(&project.config.image).await?;
    let image_dir = ensure_image(&mut resolved).await?;
    let bundle_path = ensure_rootfs(&image_dir, &resolved.digest, &project.hash)?;

    // Apply project overrides to the base bundle
    write_config(
        &bundle_path,
        &project.cwd,
        &resolved.image_config,
        vm.binds(),
        &cli.args,
        terminal.is_tty(),
    )?;
    install_ca_cert(&image_dir, &bundle_path, &project.ca_cert)?;

    Ok(Bundle { path: bundle_path })
}

async fn resolve(image_ref: &str) -> anyhow::Result<ResolvedImage> {
    eprintln!("Resolving image {image_ref}...");

    if let Some(image_id) = docker::image_exists(image_ref) {
        eprintln!(
            "  found locally via docker: {}",
            &image_id[..19.min(image_id.len())]
        );
        return Ok(ResolvedImage {
            digest: image_id,
            image_config: Default::default(),
            source: ImageSource::Docker {
                image_ref: image_ref.to_string(),
            },
        });
    }

    let reg = registry::resolve(image_ref).await?;
    Ok(ResolvedImage {
        digest: reg.digest.clone(),
        image_config: reg.image_config.clone(),
        source: ImageSource::Registry(reg),
    })
}

struct ResolvedImage {
    digest: String,
    image_config: oci_client::config::ConfigFile,
    source: ImageSource,
}

enum ImageSource {
    Docker { image_ref: String },
    Registry(registry::RegistryImage),
}

async fn ensure_image(resolved: &mut ResolvedImage) -> anyhow::Result<PathBuf> {
    let dir = cache::image_dir(&resolved.digest)?;
    let rootfs = dir.join("rootfs");

    if rootfs.exists() {
        if matches!(resolved.source, ImageSource::Docker { .. }) {
            let config_path = dir.join("image_config.json");
            if config_path.exists() {
                let data = std::fs::read(&config_path)?;
                resolved.image_config = serde_json::from_slice(&data)?;
            }
        }
        eprintln!("  image cached");
        return Ok(dir);
    }

    std::fs::create_dir_all(&dir)?;

    match &resolved.source {
        ImageSource::Docker { image_ref } => {
            eprintln!("  exporting from docker...");
            resolved.image_config =
                docker::save_and_extract(image_ref, &rootfs, &dir.join("image_config.json"))?;
        }
        ImageSource::Registry(reg) => {
            eprintln!("Downloading image layers...");
            let layers = &reg.manifest.layers;
            let mut layer_paths = Vec::new();
            for (i, layer_desc) in layers.iter().enumerate() {
                let short_digest = &layer_desc.digest[7..19];
                eprintln!(
                    "  layer {}/{}: {} ({})",
                    i + 1,
                    layers.len(),
                    short_digest,
                    format_size(layer_desc.size)
                );
                let layer_path = dir.join(format!("layer_{i}.tar.gz"));
                if !layer_path.exists() {
                    registry::pull_layer(&reg.reference, layer_desc, &layer_path).await?;
                }
                layer_paths.push(layer_path);
            }

            eprintln!("  extracting layers...");
            let layer_refs: Vec<&Path> = layer_paths.iter().map(|p| p.as_path()).collect();
            layer::extract_layers(&layer_refs, &rootfs)?;

            let config_json = serde_json::to_string_pretty(&resolved.image_config)?;
            std::fs::write(dir.join("image_config.json"), config_json)?;
        }
    }

    eprintln!("  image ready");
    Ok(dir)
}

/// Ensure the project bundle rootfs exists (CoW copy of image).
fn ensure_rootfs(
    image_dir: &Path,
    image_digest: &str,
    project_hash: &str,
) -> anyhow::Result<PathBuf> {
    let project = cache::project_dir(project_hash)?;
    let bundle = project.join("bundle");
    let digest_file = project.join("image_digest");

    if bundle.exists() {
        if let Ok(stored) = std::fs::read_to_string(&digest_file) {
            if stored.trim() != image_digest {
                eprintln!("  image changed, recreating project bundle...");
                std::fs::remove_dir_all(&bundle)?;
            }
        }
    }

    if !bundle.join("rootfs").exists() {
        eprintln!("  creating project bundle...");
        std::fs::create_dir_all(&bundle)?;
        cache::cow_copy(&image_dir.join("rootfs"), &bundle.join("rootfs"))?;
        std::fs::create_dir_all(&project)?;
        std::fs::write(&digest_file, image_digest)?;
    }

    Ok(bundle)
}

fn write_config(
    bundle_path: &Path,
    cwd: &Path,
    image_config: &oci_client::config::ConfigFile,
    binds: &[ContainerBind],
    user_args: &[String],
    terminal: bool,
) -> anyhow::Result<()> {
    config::generate_config(
        &image_config,
        &cwd,
        binds,
        user_args,
        terminal,
        &bundle_path.join("config.json"),
    )
}

/// Rewrite the container's trust store with the image's original certs
/// plus the project CA cert for TLS MITM.
fn install_ca_cert(
    image_dir: &Path,
    bundle_path: &Path,
    ca_cert_path: &Path,
) -> anyhow::Result<()> {
    let ca_store = "rootfs/etc/ssl/certs/ca-certificates.crt";
    let dest = bundle_path.join(ca_store);
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Read from the pristine image, not the bundle (avoids duplicating on repeated runs)
    let existing = std::fs::read(image_dir.join(ca_store)).unwrap_or_default();
    let ca_cert = std::fs::read(ca_cert_path)?;
    let mut out = existing;
    if !out.ends_with(b"\n") && !out.is_empty() {
        out.push(b'\n');
    }
    out.extend_from_slice(&ca_cert);
    std::fs::write(&dest, &out)?;
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
