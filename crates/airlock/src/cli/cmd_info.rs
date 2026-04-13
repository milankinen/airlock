//! `airlock info` — display project details.

use crate::{cli, project};

/// Print project metadata (ID, path, status, image, config) to stdout.
pub fn run(path: Option<&str>) -> i32 {
    let project = match project::load(path) {
        Ok(p) => p,
        Err(e) => {
            cli::error!("{e:#}");
            return 1;
        }
    };

    if !project.cache_dir.exists() {
        cli::error!(
            "no project data for {} — run `airlock up` first",
            project.host_cwd.display()
        );
        return 1;
    }

    let status = if project.is_running() {
        cli::red("running")
    } else {
        cli::dim("stopped")
    };

    println!("ID:       {}", project.id());
    println!("Path:     {}", project.display_cwd());
    println!("Status:   {status}");
    println!("Image:    {}", project.config.vm.image);
    println!("CPUs:     {}", project.config.vm.cpus);
    println!("Memory:   {}", project.config.vm.memory);

    if let Some(ago) = project.last_run_ago() {
        println!("Last run: {ago}");
    }

    println!("Disk:     {}", project.cache_dir.display());

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
            println!("  {key}: {} → {}{status}", mount.source, mount.target);
        }
    }

    if !project.config.network.rules.is_empty() {
        let default_mode = format!("{:?}", project.config.network.default_mode).to_lowercase();
        println!("Network rules (default: {default_mode}):");
        for (key, rule) in &project.config.network.rules {
            let status = if rule.enabled { "" } else { " (disabled)" };
            let mw = if rule.middleware.is_empty() {
                String::new()
            } else {
                format!(", {} middleware", rule.middleware.len())
            };
            println!(
                "  {key}: allow {} deny {}{mw}{status}",
                rule.allow.len(),
                rule.deny.len()
            );
        }
    }

    0
}
