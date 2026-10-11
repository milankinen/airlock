//! Sandbox preparation and boot reports.
//!
//! Shows the terminal output while a sandbox is prepared and booted.

use std::path::Path;

use crate::messages::sandbox as msg;
use crate::{cli, daemon, masking, project};

/// Print the "Preparing sandbox" header.
/// Args:
///  - `sandbox_dir`: Sandbox directory. If it has no CA certificate yet,
///    the output tells that a new one is generated ([`project::open`]
///    generates it).
///  - `image_name`: Image name to show
pub fn print_preparing(sandbox_dir: &Path, image_name: &str) {
    cli::log!("{}", msg::PREPARING_SANDBOX);
    cli::log!(
        "  {}",
        msg::config_loaded(cli::check(), cli::dim(image_name))
    );
    if !project::has_ca(sandbox_dir) {
        cli::log!("  {}", msg::ca_cert_generated(cli::check()));
    }
}

/// Print the enabled env, mounts, network rules, socket forwards, TCP port
/// forwards, daemons and masks in verbose mode, grouped by kind.
pub(super) fn print_mounts_and_rules(project: &project::Project) {
    if !project.env.is_empty() {
        cli::verbose!(
            "  {}",
            msg::env_summary(cli::bullet(), project.env.len(), project.env.masked_count())
        );
    }
    let enabled_mounts: Vec<_> = project
        .config
        .mounts
        .iter()
        .filter(|(_, m)| m.enabled)
        .collect();
    if !enabled_mounts.is_empty() {
        cli::verbose!(
            "  {}",
            msg::mounts_summary(cli::bullet(), enabled_mounts.len())
        );
        for (key, mount) in &enabled_mounts {
            cli::verbose!(
                "      {}",
                msg::mount_entry(key, &mount.source, &mount.target)
            );
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
            "  {}",
            msg::network_rules_summary(cli::bullet(), enabled_rules.len(), policy)
        );
        for (key, rule) in &enabled_rules {
            // Count only the entries that have a value.
            let injected = rule
                .inject
                .iter()
                .filter(|name| !project.env.is_omitted(name))
                .count();
            let inject = if injected == 0 {
                String::new()
            } else {
                msg::rule_inject_suffix(injected)
            };
            cli::verbose!(
                "      {}",
                msg::network_rule_entry(key, rule.allow.len(), rule.deny.len(), inject)
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
        cli::verbose!(
            "  {}",
            msg::sockets_summary(cli::bullet(), enabled_sockets.len())
        );
        for (key, sock) in &enabled_sockets {
            cli::verbose!(
                "      {}",
                msg::mount_entry(key, &sock.host.source, &sock.host.target)
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
        cli::verbose!(
            "  {}",
            msg::port_forwards_summary(cli::bullet(), enabled_ports.len())
        );
        for (key, pf) in &enabled_ports {
            for m in &pf.host {
                cli::verbose!("      {}", msg::port_forward_to_host(key, m.host, m.guest));
            }
            for m in &pf.guest {
                cli::verbose!("      {}", msg::port_forward_to_guest(key, m.host, m.guest));
            }
        }
    }

    daemon::print_verbose(project);
    masking::print_verbose(project);
}
