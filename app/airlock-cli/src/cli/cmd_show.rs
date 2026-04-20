//! `airlock show` — display sandbox details.

use clap::Args;

use crate::vault::Vault;
use crate::{cli, project};

/// CLI arguments for `airlock show`.
#[derive(Args, Debug)]
pub struct ShowArgs {}

/// Print sandbox metadata (path, status, image, config) to stdout.
pub fn main(_args: &ShowArgs, vault: Vault) -> i32 {
    let project = match project::load(vault) {
        Ok(s) => s,
        Err(e) => {
            cli::error!("Sandbox details loading failed: {e:#}");
            return 1;
        }
    };

    if !project.sandbox_dir.exists() {
        cli::error!(
            "No sandbox for {} — run `airlock start` first",
            project.host_cwd.display()
        );
        return 1;
    }

    let status = if project.is_running() {
        cli::red("running")
    } else {
        cli::dim("stopped")
    };

    println!("Path:     {}", project.display_cwd());
    println!("Status:   {status}");
    println!("Image:    {}", project.config.vm.image);
    println!("CPUs:     {}", project.config.vm.cpus);
    println!("Memory:   {}", project.config.vm.memory);

    if let Some(ago) = project.last_run_ago() {
        println!("Last run: {ago}");
    }

    println!("Sandbox:  {}", project.sandbox_dir.display());

    if let Some((used, total)) = project.disk_usage() {
        println!(
            "Disk:     {} / {}",
            cli::format_bytes(used),
            cli::format_bytes(total)
        );
    }

    if !project.config.disk.cache.is_empty() {
        println!("Disk cache:");
        for (key, mount) in &project.config.disk.cache {
            let status = if mount.enabled { "" } else { " (disabled)" };
            println!("  {key}: {}{status}", mount.paths.join(", "));
        }
    }

    if !project.config.mounts.is_empty() {
        println!("Mounts:");
        for (key, mount) in &project.config.mounts {
            let status = if mount.enabled { "" } else { " (disabled)" };
            println!(
                "  {key}: {} \u{2192} {}{status}",
                mount.source, mount.target
            );
        }
    }

    {
        let policy = format!("{:?}", project.config.network.policy).to_lowercase();
        println!("Network policy: {policy}");
    }

    if !project.config.network.rules.is_empty() {
        println!("Network rules:");
        for (key, rule) in &project.config.network.rules {
            let status = if rule.enabled { "" } else { " (disabled)" };
            println!(
                "  {key}: allow {} deny {}{status}",
                rule.allow.len(),
                rule.deny.len()
            );
        }
    }

    if !project.config.network.middleware.is_empty() {
        println!("Network middleware:");
        for (key, mw) in &project.config.network.middleware {
            let status = if mw.enabled { "" } else { " (disabled)" };
            println!("  {key}: {} targets{status}", mw.target.len());
        }
    }

    0
}
