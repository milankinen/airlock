pub(crate) mod config;
mod docker;
mod layer;
mod registry;

use std::path::{Path, PathBuf};

use oci_client::config::ConfigFile;

use crate::cli;
use crate::cli::CliArgs;
use crate::project::Project;
use crate::terminal::Terminal;
use crate::vm::Vm;
use crate::vm::mounts::ContainerBind;

pub struct Bundle {
    pub path: PathBuf,
}

/// Resolve, download, and prepare the OCI bundle for the project.
pub async fn prepare(
    cli: &CliArgs,
    project: &Project,
    terminal: &Terminal,
    vm: &Vm,
) -> anyhow::Result<Bundle> {
    let mut resolved = resolve(&project.config.image).await?;
    let image_dir = ensure_image(&mut resolved).await?;
    let bundle_path = ensure_rootfs(&image_dir, &resolved.digest, project)?;

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
            image_config: ConfigFile::default(),
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
        image_config: reg.image_config.clone(),
        source: ImageSource::Registry(Box::new(reg)),
    })
}

struct ResolvedImage {
    digest: String,
    image_config: oci_client::config::ConfigFile,
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
                resolved.image_config = serde_json::from_slice(&data)?;
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
            resolved.image_config =
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

            let config_json = serde_json::to_string_pretty(&resolved.image_config)?;
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
    let bundle = project.dir.join("bundle");
    let digest_file = project.dir.join("image_digest");
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

fn write_config(
    bundle_path: &Path,
    cwd: &Path,
    image_config: &oci_client::config::ConfigFile,
    binds: &[ContainerBind],
    user_args: &[String],
    terminal: bool,
) -> anyhow::Result<()> {
    config::generate_config(
        image_config,
        cwd,
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
