//! Terminal reports printed while a sandbox is prepared and booted.

use std::path::Path;

use crate::{cli, daemon, masking, project};

/// Print the "Preparing sandbox" header with the image name, and whether
/// the sandbox at `sandbox_dir` gets a new CA certificate (it has none
/// yet; [`project::open`] generates it).
pub fn print_preparing(sandbox_dir: &Path, image_name: &str) {
    cli::log!("Preparing sandbox...");
    cli::log!(
        "  {} config loaded, image: {}",
        cli::check(),
        cli::dim(image_name)
    );
    if !project::has_ca(sandbox_dir) {
        cli::log!("  {} ca cert generated", cli::check());
    }
}

/// Verbose-only: list enabled mounts, network rules, socket forwards,
/// and TCP port forwards grouped by kind.
pub(super) fn print_mounts_and_rules(project: &project::Project) {
    if !project.env.is_empty() {
        cli::verbose!(
            "  {} env: {} vars ({} masked)",
            cli::bullet(),
            project.env.len(),
            project.env.masked_count()
        );
    }
    let enabled_mounts: Vec<_> = project
        .config
        .mounts
        .iter()
        .filter(|(_, m)| m.enabled)
        .collect();
    if !enabled_mounts.is_empty() {
        cli::verbose!("  {} mounts: {}", cli::bullet(), enabled_mounts.len());
        for (key, mount) in &enabled_mounts {
            cli::verbose!("      {key}: {} \u{2192} {}", mount.source, mount.target);
        }
    }
    let enabled_rules: Vec<_> = project
        .config
        .network
        .rules
        .iter()
        .filter(|(_, r)| r.enabled)
        .collect();
    if !enabled_rules.is_empty() {
        let policy = project.config.network.policy.label();
        cli::verbose!(
            "  {} network rules: {} (policy: {policy})",
            cli::bullet(),
            enabled_rules.len()
        );
        for (key, rule) in &enabled_rules {
            let inject = if rule.inject.is_empty() {
                String::new()
            } else {
                format!(" inject {}", rule.inject.len())
            };
            cli::verbose!(
                "      {key}: allow {} deny {}{inject}",
                rule.allow.len(),
                rule.deny.len()
            );
        }
    }

    let enabled_sockets: Vec<_> = project
        .config
        .network
        .sockets
        .iter()
        .filter(|(_, s)| s.enabled)
        .collect();
    if !enabled_sockets.is_empty() {
        cli::verbose!("  {} sockets: {}", cli::bullet(), enabled_sockets.len());
        for (key, sock) in &enabled_sockets {
            cli::verbose!(
                "      {key}: {} \u{2192} {}",
                sock.host.source,
                sock.host.target
            );
        }
    }

    let enabled_ports: Vec<_> = project
        .config
        .network
        .ports
        .iter()
        .filter(|(_, p)| p.enabled && !(p.host.is_empty() && p.guest.is_empty()))
        .collect();
    if !enabled_ports.is_empty() {
        cli::verbose!("  {} port forwards: {}", cli::bullet(), enabled_ports.len());
        for (key, pf) in &enabled_ports {
            for m in &pf.host {
                cli::verbose!("      {key}: host :{} \u{2190} guest :{}", m.host, m.guest);
            }
            for m in &pf.guest {
                cli::verbose!("      {key}: host :{} \u{2192} guest :{}", m.host, m.guest);
            }
        }
    }

    daemon::print_verbose(project);
    masking::print_verbose(project);
}
