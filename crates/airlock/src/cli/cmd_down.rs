//! `airlock down` — delete a project's cached state.

use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;

use crate::{cli, project};

/// Remove the project cache directory after confirmation (unless `--force`).
pub fn run(path: Option<&str>, force: bool) -> i32 {
    let project = match project::load(path) {
        Ok(p) => p,
        Err(e) => {
            cli::error!("{e:#}");
            return 1;
        }
    };

    if !project.cache_dir.exists() {
        cli::error!("no project found for {}", project.host_cwd.display());
        return 1;
    }

    if project.is_running() {
        cli::error!("project is running, stop it first");
        return 1;
    }

    if !force {
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt(format!(
                "Remove project for {}?",
                project.host_cwd.display()
            ))
            .items(["Yes", "No"])
            .default(1)
            .interact()
            .unwrap_or(1);
        if selection != 0 {
            println!("Aborted.");
            return 0;
        }
    }

    if let Err(e) = std::fs::remove_dir_all(&project.cache_dir) {
        cli::error!("failed to remove project: {e}");
        return 1;
    }

    println!("Removed project for {}", project.host_cwd.display());
    0
}
