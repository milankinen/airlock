//! The `airlock sandbox` command.
//!
//! Lists the sandboxes in the airlock data directory, shows the details of
//! one sandbox, and removes sandboxes. Without an id, `info` and `remove`
//! use the sandbox of the current directory, as `airlock info` and
//! `airlock rm` do. Sandboxes in a project directory are not in the list.

use std::path::Path;

use clap::{Args, Subcommand};

use super::cmd_info::{self, Details, find_by_id};
use super::cmd_rm::{self, RmArgs};
use crate::cli::prompt::yes_no::YesNo;
use crate::context::Context;
use crate::sandboxes::{self, Found, Location, registry};
use crate::{cli, oci, project};

/// CLI arguments for `airlock sandbox`.
#[derive(Args, Debug)]
pub struct SandboxArgs {
    #[command(subcommand)]
    cmd: SandboxCmd,
}

/// Subcommands of `airlock sandbox`.
#[derive(Subcommand, Debug)]
enum SandboxCmd {
    /// List the sandboxes in the airlock data directory
    #[command(alias = "ls")]
    List,
    /// Show the details of a sandbox (default: the sandbox of the current directory)
    Info {
        /// Sandbox id (see `airlock sandbox list`)
        id: Option<String>,
        /// Print the details as JSON
        #[arg(long)]
        json: bool,
    },
    /// Remove sandboxes from the airlock data directory (default: the sandbox of the current directory)
    #[command(alias = "rm")]
    Remove {
        /// Sandbox ids (see `airlock sandbox list`)
        ids: Vec<String>,
        /// Do not ask for confirmation
        #[arg(short = 'f', long)]
        force: bool,
    },
}

/// Entry point for `airlock sandbox`.
/// Returns:
///   Process exit code: 0 on success or abort, 1 on error.
pub async fn main(args: SandboxArgs, context: &Context) -> i32 {
    let result = match args.cmd {
        SandboxCmd::List => list(context).await,
        SandboxCmd::Info { id, json } => {
            Ok(cmd_info::run(context.clone(), id.as_deref(), json).await)
        }
        SandboxCmd::Remove { ids, force } if ids.is_empty() => {
            Ok(cmd_rm::main(&RmArgs { force }, context).await)
        }
        SandboxCmd::Remove { ids, force } => remove(context, &ids, force).await,
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            cli::error!("{e:#}");
            1
        }
    }
}

/// Print the table of the registered sandboxes.
async fn list(context: &Context) -> anyhow::Result<i32> {
    let entries = registry::list(&context.db).await?;
    if entries.is_empty() {
        cli::log!("No sandboxes in {}", context.boxes_dir().display());
        return Ok(0);
    }
    let mut rows = Vec::new();
    for entry in entries {
        let found = Found {
            location: Location::DataDir {
                id: entry.id.clone(),
            },
            dir: context.boxes_dir().join(&entry.id),
            project: entry.project,
        };
        let details = Details::read(&found);
        let mut project = details.project.display().to_string();
        if !details.project.is_dir() {
            project.push_str(" (missing)");
        }
        rows.push([
            entry.id,
            details.status.to_string(),
            details
                .last_run
                .map_or_else(|| "never".to_string(), project::time_ago),
            details
                .disk_used
                .map_or_else(String::new, cli::format_bytes),
            project,
        ]);
    }
    let header = ["ID", "STATUS", "LAST RUN", "DISK", "PROJECT"];
    let widths: Vec<usize> = (0..header.len() - 1)
        .map(|i| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let print_row = |cells: [&str; 5]| {
        let [a, b, c, d, project] = cells;
        let [wa, wb, wc, wd] = [widths[0], widths[1], widths[2], widths[3]];
        println!("{a:<wa$}  {b:<wb$}  {c:<wc$}  {d:<wd$}  {project}");
    };
    print_row(header);
    for row in &rows {
        print_row(row.each_ref().map(String::as_str));
    }
    Ok(0)
}

/// Remove the registered sandboxes `ids`, each after a confirmation (unless
/// `force`). Then remove the images that no sandbox uses.
/// Returns:
///   Exit code 1 if a sandbox was not removed because of an error.
async fn remove(context: &Context, ids: &[String], force: bool) -> anyhow::Result<i32> {
    let mut code = 0;
    let mut removed = false;
    for id in ids {
        match remove_one(context, id, force).await {
            Ok(true) => removed = true,
            Ok(false) => {}
            Err(e) => {
                cli::error!("{e:#}");
                code = 1;
            }
        }
    }
    if removed {
        oci::gc_sweep(&context.data_dir);
    }
    Ok(code)
}

/// Remove the registered sandbox `id` after a confirmation (unless
/// `force`).
/// Returns:
///   `true` if the sandbox was removed, `false` if the user said no.
async fn remove_one(context: &Context, id: &str, force: bool) -> anyhow::Result<bool> {
    let found = find_by_id(context, id).await?;
    // Fail before the question. `remove_box` checks again under the lock.
    anyhow::ensure!(
        !project::is_running(&found.dir),
        "Sandbox {id} is running, stop it first"
    );
    if !force && !confirm(id, &found.project) {
        cli::error!("Aborted ({id}).");
        return Ok(false);
    }
    sandboxes::remove_box(context, id).await?;
    cli::log!("Sandbox {id} removed");
    Ok(true)
}

/// Ask to confirm the removal of the sandbox `id` of `project`.
/// Returns:
///   True if confirmed. Esc, no terminal or a failed prompt is "no".
fn confirm(id: &str, project: &Path) -> bool {
    let question = format!("Remove sandbox {id} of {}?", project.display());
    let question = YesNo {
        question: &question,
        default: false,
    };
    matches!(question.ask(), Ok(Some(true)))
}
