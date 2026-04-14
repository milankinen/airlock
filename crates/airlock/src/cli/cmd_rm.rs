//! `airlock rm` — delete a sandbox's cached state.

use clap::Args;
use dialoguer::Select;
use dialoguer::theme::ColorfulTheme;

use crate::{cli, project};

/// CLI arguments for `airlock rm`.
#[derive(Args, Debug)]
pub struct RmArgs {
    /// Skip confirmation prompt
    #[arg(short = 'f', long)]
    pub force: bool,
}

/// Remove the sandbox directory after confirmation (unless `--force`).
pub fn run(args: &RmArgs) -> i32 {
    let project = match project::load() {
        Ok(s) => s,
        Err(e) => {
            cli::error!("{e:#}");
            return 1;
        }
    };

    if !project.cache_dir.exists() {
        return 0;
    }

    if project.is_running() {
        cli::error!("Sandbox is running, stop it first");
        return 1;
    }

    if !args.force {
        let selection = Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Remove project data?")
            .items(["Yes", "No"])
            .default(1)
            .interact()
            .unwrap_or(1);
        if selection != 0 {
            cli::error!("Aborted.");
            return 0;
        }
    }

    if let Err(e) = std::fs::remove_dir_all(&project.cache_dir) {
        cli::error!("Failed to remove project data: {e}");
        return 1;
    }

    cli::log!("Sandbox removed");
    0
}
