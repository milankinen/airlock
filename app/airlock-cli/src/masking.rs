//! Host-side helpers for `[mask.<name>]` directory-masking blocks.
//!
//! Validates that each `paths` entry is a plain project-relative path
//! (no leading `/` or `~`, no `..` segments), filters out disabled
//! masks, and converts the config to the wire format the guest
//! supervisor consumes.

use crate::{cli, project, rpc};

/// Validate config-level mask blocks and translate them into
/// wire-format `MaskSpec`s. Disabled entries are filtered out.
pub fn build_specs(project: &project::Project) -> anyhow::Result<Vec<rpc::MaskSpec>> {
    project
        .config
        .mask
        .iter()
        .filter(|(_, m)| m.enabled)
        .map(|(name, m)| {
            for (idx, path) in m.paths.iter().enumerate() {
                validate_path(name, idx, path)?;
            }
            Ok(rpc::MaskSpec {
                name: name.clone(),
                paths: m.paths.clone(),
            })
        })
        .collect()
}

fn validate_path(name: &str, idx: usize, path: &str) -> anyhow::Result<()> {
    if path.is_empty() {
        anyhow::bail!("mask.{name}.paths[{idx}]: empty path");
    }
    if path.starts_with('/') {
        anyhow::bail!(
            "mask.{name}.paths[{idx}]: absolute paths are not allowed (got `{path}`); paths must be project-relative"
        );
    }
    if path.starts_with('~') {
        anyhow::bail!(
            "mask.{name}.paths[{idx}]: home-relative paths are not allowed (got `{path}`); paths must be project-relative"
        );
    }
    for seg in path.split('/') {
        if seg == ".." {
            anyhow::bail!(
                "mask.{name}.paths[{idx}]: `..` is not allowed in mask paths (got `{path}`)"
            );
        }
    }
    Ok(())
}

/// Verbose-only summary of declared masks. Disabled entries are
/// filtered out. Mirrors `daemon::print_verbose`.
pub fn print_verbose(project: &project::Project) {
    let enabled: Vec<_> = project
        .config
        .mask
        .iter()
        .filter(|(_, m)| m.enabled)
        .collect();
    if enabled.is_empty() {
        return;
    }
    cli::verbose!("  {} masks: {}", cli::bullet(), enabled.len());
    for (name, m) in &enabled {
        cli::verbose!(
            "      {name}: {} path(s): {}",
            m.paths.len(),
            m.paths.join(", ")
        );
    }
}
