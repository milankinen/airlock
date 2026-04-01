pub mod cache;
mod config;
mod docker;
mod layer;
mod registry;

use std::path::{Path, PathBuf};

pub struct ResolvedImage {
    pub digest: String,
    pub image_config: oci_client::config::ConfigFile,
    source: ImageSource,
}

enum ImageSource {
    Docker { image_ref: String },
    Registry(registry::RegistryImage),
}

/// Resolve an image: try local Docker first, then registry.
pub async fn resolve(image_ref: &str) -> anyhow::Result<ResolvedImage> {
    eprintln!("Resolving image {image_ref}...");

    // 1. Try local Docker daemon
    if let Some(image_id) = docker::image_exists(image_ref) {
        eprintln!("  found locally via docker: {}", &image_id[..19.min(image_id.len())]);
        return Ok(ResolvedImage {
            digest: image_id,
            image_config: Default::default(), // will be read during export
            source: ImageSource::Docker {
                image_ref: image_ref.to_string(),
            },
        });
    }

    // 2. Try OCI registry
    let reg = registry::resolve(image_ref).await?;
    Ok(ResolvedImage {
        digest: reg.digest.clone(),
        image_config: reg.image_config.clone(),
        source: ImageSource::Registry(reg),
    })
}

/// Ensure image is downloaded/exported and extracted to cache.
pub async fn ensure_image(resolved: &mut ResolvedImage) -> anyhow::Result<PathBuf> {
    let dir = cache::image_dir(&resolved.digest)?;
    let rootfs = dir.join("rootfs");

    if rootfs.exists() {
        // Load cached config if we came from Docker path (config was deferred)
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
            let config = docker::save_and_extract(
                image_ref,
                &rootfs,
                &dir.join("image_config.json"),
            )?;
            resolved.image_config = config;
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

/// Ensure the project bundle exists (CoW copy of image rootfs).
/// Regenerates config.json every run to pick up mount changes.
pub fn ensure_project(
    image_dir: &Path,
    image_config: &oci_client::config::ConfigFile,
    image_digest: &str,
    project_hash: &str,
    binds: &[crate::mounts::ContainerBind],
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

        let src_rootfs = image_dir.join("rootfs");
        let dst_rootfs = bundle.join("rootfs");
        cache::cow_copy(&src_rootfs, &dst_rootfs)?;

        std::fs::create_dir_all(&project)?;
        std::fs::write(&digest_file, image_digest)?;
    }

    // Regenerate config.json every run (mounts may change)
    config::generate_config(image_config, binds, &bundle.join("config.json"))?;

    Ok(bundle)
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
