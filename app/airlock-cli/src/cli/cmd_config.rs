//! The `airlock config` command.
//!
//! Prints the config of a project, so that the user can save it to a file.
//! By default it is the project config: the project files and the local
//! project config, without the user files. With `--resolved` it is the
//! final config of the last run, after all files and packs merge.
//! `airlock sandboxes config` gives the same output for any sandbox.

use clap::Args;

use super::cmd_info::find;
use crate::config::export::{self, Format};
use crate::context::Context;
use crate::sandboxes::{self, Found};
use crate::util::verified_path::VerifiedPath;
use crate::{cli, config, messages, packs, project};

/// CLI arguments for `airlock config`.
#[derive(Args, Debug)]
pub struct ConfigArgs {
    /// Print the resolved config of the last run, with the user files and packs merged in
    #[arg(long)]
    pub resolved: bool,
    /// Output format
    #[arg(long, value_enum, default_value_t = Format::Toml)]
    pub format: Format,
}

/// Print the config of the project of the current directory (see [`run`]).
/// Returns:
///   Process exit code: 0 on success, 1 on error.
pub async fn main(args: &ConfigArgs, context: &Context) -> i32 {
    run(context, None, args).await
}

/// Print the config of the sandbox `id`, or of the project of the current
/// directory, to stdout. Errors and warnings go to stderr. A project
/// without a config gives a warning and an empty document.
/// Returns:
///   Process exit code: 0 on success, 1 on error.
pub(super) async fn run(context: &Context, id: Option<&str>, args: &ConfigArgs) -> i32 {
    let text = document(context, id, args.resolved)
        .await
        .and_then(|document| export::render(&document, args.format));
    match text {
        Ok(text) => {
            print!("{text}");
            0
        }
        Err(e) => {
            cli::error!("{}", messages::cli::config_export_failed(format!("{e:#}")));
            1
        }
    }
}

/// Get the config document to print (see [`run`]).
async fn document(
    context: &Context,
    id: Option<&str>,
    resolved: bool,
) -> anyhow::Result<serde_json::Value> {
    let (project_dir, found) = find(context, id).await?;
    if resolved {
        return resolved_document(context, &project_dir, found.as_ref()).await;
    }
    // Same rule as `sandboxes::find_local_config_dir`. The sandbox is
    // known already.
    let local_dir = match &found {
        Some(found) => sandboxes::local_config_dir(&found.dir)?,
        None => project_dir.join(".airlock")?,
    };
    let document = config::load(&project_dir, &local_dir)?.project_document();
    // No config is not an error. The output stays a valid empty document,
    // thus a script that saves it gets an empty config.
    Ok(document.unwrap_or_else(|| {
        cli::log!(
            "{} {}",
            cli::yellow("!"),
            messages::cli::no_project_config(&project_dir)
        );
        serde_json::Value::Object(serde_json::Map::new())
    }))
}

/// Resolve the stored config of the last run of the sandbox `found` of
/// `project_dir`.
/// Returns:
///   The resolved config document, or an error if the sandbox has no
///   stored config or the config does not resolve any more.
async fn resolved_document(
    context: &Context,
    project_dir: &VerifiedPath,
    found: Option<&Found>,
) -> anyhow::Result<serde_json::Value> {
    let stored = found.and_then(|found| project::read_run_meta(&found.dir).last_run_config);
    let Some(stored) = stored else {
        anyhow::bail!(messages::cli::no_last_run_config(project_dir));
    };
    let layered: config::LayeredConfig = serde_json::from_value(stored)?;
    // The resolve runs the `config.lua` of the packs. This can make the
    // pack directories in the data directory, as `start` does.
    let packs = packs::init(&context.settings.packs, &context.home)?;
    let resolved = layered
        .resolve(&config::ResolveInput {
            data_dir: &context.data,
            packs: &packs,
            overrides: &config::ConfigOverrides::default(),
            project_root: Some(project_dir),
            // Only `airlock start` asks about new or changed packs.
            approval: crate::packs::approval::ApprovalMode::Refuse,
        })
        .await?;
    export::resolved_document(&resolved)
}
