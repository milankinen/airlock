pub mod cache;
mod config;
mod layer;
mod registry;

use std::path::{Path, PathBuf};

pub use registry::ResolvedImage;

pub async fn resolve(image_ref: &str) -> anyhow::Result<ResolvedImage> {
    eprintln!("Resolving image {image_ref}...");
    registry::resolve(image_ref).await
}

/// Ensure image layers are downloaded and extracted to cache.
pub async fn ensure_image(resolved: &ResolvedImage) -> anyhow::Result<PathBuf> {
    let dir = cache::image_dir(&resolved.digest)?;
    let rootfs = dir.join("rootfs");

    if rootfs.exists() {
        eprintln!("  image cached");
        return Ok(dir);
    }

    eprintln!("Downloading image layers...");
    std::fs::create_dir_all(&dir)?;

    let layers = &resolved.manifest.layers;
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
            registry::pull_layer(&resolved.reference, layer_desc, &layer_path).await?;
        }
        layer_paths.push(layer_path);
    }

    eprintln!("  extracting layers...");
    let layer_refs: Vec<&Path> = layer_paths.iter().map(|p| p.as_path()).collect();
    layer::extract_layers(&layer_refs, &rootfs)?;

    let config_json = serde_json::to_string_pretty(&resolved.image_config)?;
    std::fs::write(dir.join("image_config.json"), config_json)?;

    eprintln!("  image ready");
    Ok(dir)
}

/// Ensure the project bundle exists (CoW copy of image rootfs).
pub fn ensure_project(
    image_dir: &Path,
    image_config: &oci_client::config::ConfigFile,
    image_digest: &str,
    project_hash: &str,
) -> anyhow::Result<PathBuf> {
    let project = cache::project_dir(project_hash)?;
    let bundle = project.join("bundle");
    let digest_file = project.join("image_digest");

    if bundle.exists() {
        if let Ok(stored) = std::fs::read_to_string(&digest_file) {
            if stored.trim() == image_digest {
                return Ok(bundle);
            }
            eprintln!("  image changed, recreating project bundle...");
            std::fs::remove_dir_all(&bundle)?;
        }
    }

    if !bundle.exists() {
        eprintln!("  creating project bundle...");
        std::fs::create_dir_all(&bundle)?;

        let src_rootfs = image_dir.join("rootfs");
        let dst_rootfs = bundle.join("rootfs");
        cache::cow_copy(&src_rootfs, &dst_rootfs)?;

        config::generate_config(image_config, &bundle.join("config.json"))?;

        std::fs::create_dir_all(&project)?;
        std::fs::write(&digest_file, image_digest)?;
    }

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
