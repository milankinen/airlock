//! Directory masking support on the host.
//!
//! Validates the `[mask.<name>]` config blocks and converts them to the form
//! that the guest supervisor uses. Also shows a summary of the masks in
//! verbose mode.

use crate::{cli, project, rpc};

/// Validate the enabled mask blocks of the project config and convert them
/// to the wire format.
/// Returns:
///   One [`rpc::MaskSpec`] for each enabled mask, or error if a path is not
///   a plain project-relative path.
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

/// Make sure that `path` is a plain project-relative path: not empty, no
/// leading `/` or `~`, and no `..` segments.
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

/// Print a summary of the enabled masks in verbose mode. Same format as
/// [`crate::daemon::print_verbose`].
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
