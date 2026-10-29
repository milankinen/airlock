//! The `airlock templates` command.
//!
//! Creates, lists, shows and removes sandbox templates. A template is a
//! sandbox disk with installed packs. The setup wizard of `airlock start`
//! offers the templates as the base of a new sandbox.

use std::fs::Metadata;
use std::os::unix::fs::MetadataExt;

use clap::{Args, Subcommand};

use crate::cli::LogLevel;
use crate::cli::prompt::choose;
use crate::context::Context;
use crate::data_dir::DataDir;
use crate::templates::{self, CONFIG_FILE, Template};
use crate::vm::disk::DISK_FILE;
use crate::{cli, messages, oci, start};

/// CLI arguments for `airlock templates`.
#[derive(Args, Debug)]
pub struct TemplateArgs {
    #[command(subcommand)]
    cmd: TemplateCmd,
}

/// Subcommands of `airlock templates`.
#[derive(Subcommand, Debug)]
enum TemplateCmd {
    /// List the templates
    #[command(alias = "ls")]
    List,
    /// Show the details of a template
    Info {
        /// Template name (see `airlock templates list`)
        name: String,
    },
    /// Create a template with the setup wizard
    Create {
        /// Template name: letters, digits, `-`, `_` or `.`
        name: String,
        /// Re-create the template if it exists
        #[arg(short = 'f', long)]
        force: bool,
        /// Log level of the install VM
        #[arg(long, env = "AIRLOCK_LOG_LEVEL", default_value = "info")]
        log_level: LogLevel,
        /// Show the install output
        #[arg(short = 'v', long)]
        verbose: bool,
    },
    /// Remove a template
    #[command(alias = "rm")]
    Remove {
        /// Template name (see `airlock templates list`)
        name: String,
        /// Do not ask for confirmation
        #[arg(short = 'f', long)]
        force: bool,
    },
}

/// Entry point for `airlock templates`.
/// Returns:
///   Process exit code: 0 on success or abort, else the exit code of the
///   error.
pub async fn main(args: TemplateArgs, context: &Context) -> i32 {
    let result = match args.cmd {
        TemplateCmd::List => {
            list(&context.data);
            Ok(0)
        }
        TemplateCmd::Info { name } => info(&context.data, &name),
        TemplateCmd::Remove { name, force } => remove(&context.data, &name, force),
        TemplateCmd::Create {
            name,
            force,
            log_level,
            verbose,
        } => start::Exit::into_result(
            Box::pin(create(context, &name, force, log_level, verbose)).await,
        ),
    };
    match result {
        Ok(code) => code,
        Err(e) => {
            cli::error!("{e:#}");
            1
        }
    }
}

/// Create the template `name` (see [`templates::create::create`]).
/// Returns:
///   Exit code 0, or the exit of the failed step.
async fn create(
    context: &Context,
    name: &str,
    force: bool,
    log_level: LogLevel,
    verbose: bool,
) -> Result<i32, start::Exit> {
    cli::set_verbose(verbose);
    start::check_data_dir(&context.data)?;
    start::check_system_requirements();
    start::init_logging(log_level);
    Box::pin(templates::create::create(
        context, name, force, log_level, verbose,
    ))
    .await?;
    Ok(0)
}

/// Print the table of the templates.
fn list(data: &DataDir) {
    let templates = templates::list(data);
    if templates.is_empty() {
        cli::log!("{}", messages::cli::NO_TEMPLATES);
        return;
    }
    let rows: Vec<[String; 4]> = templates
        .iter()
        .map(|t| {
            [
                t.name.clone(),
                t.config.image.clone().map_or_else(String::new, short_image),
                disk_used(t).map_or_else(String::new, cli::format_bytes),
                pack_names(t),
            ]
        })
        .collect();
    let header = messages::cli::TEMPLATE_LIST_HEADER;
    let widths: Vec<usize> = (0..header.len() - 1)
        .map(|i| {
            rows.iter()
                .map(|row| row[i].chars().count())
                .chain([header[i].len()])
                .max()
                .unwrap_or(0)
        })
        .collect();
    let print_row = |cells: [&str; 4]| {
        let [a, b, c, packs] = cells;
        let [wa, wb, wc] = [widths[0], widths[1], widths[2]];
        println!("{a:<wa$}  {b:<wb$}  {c:<wc$}  {packs}");
    };
    print_row(header);
    for row in &rows {
        print_row(row.each_ref().map(String::as_str));
    }
}

/// Print the details of the template `name`.
fn info(data: &DataDir, name: &str) -> anyhow::Result<i32> {
    let template = templates::find(data, name)?;
    let config = &template.config;
    println!("{}", messages::cli::template_info_name(&template.name));
    println!("{}", messages::cli::template_info_directory(&template.dir));
    if let Some(meta) = file_meta(&template, CONFIG_FILE)
        && let Ok(created) = meta.modified()
    {
        println!(
            "{}",
            messages::cli::template_info_created(cli::format_local_time(created))
        );
    }
    if let Some(image) = &config.image {
        println!("{}", messages::cli::template_info_image(image));
    }
    if let Some(meta) = file_meta(&template, DISK_FILE) {
        let used = meta.blocks() * 512;
        println!(
            "{}",
            messages::cli::template_info_disk(
                cli::format_bytes(used),
                cli::format_bytes(meta.len())
            )
        );
    }
    if let Some(clipboard) = config.clipboard {
        let on = |b: bool| {
            if b {
                messages::cli::YES
            } else {
                messages::cli::NO
            }
        };
        println!(
            "{}",
            messages::cli::template_info_clipboard(on(clipboard.copy), on(clipboard.paste))
        );
    }
    if !config.packs.is_empty() {
        println!("{}", messages::cli::INFO_PACKS);
        for pack in &config.packs {
            let args: Vec<String> = pack
                .args
                .iter()
                .map(|(key, value)| match value {
                    serde_json::Value::String(s) => format!("{key} = {s}"),
                    other => format!("{key} = {other}"),
                })
                .collect();
            let head = crate::packs::id_with_version(&pack.name, pack.version.as_deref());
            if args.is_empty() {
                println!("  {head}");
            } else {
                println!("  {head} ({})", args.join(", "));
            }
        }
    }
    Ok(0)
}

/// Remove the template `name` after a confirmation (unless `force`). Then
/// remove the images that nothing uses.
fn remove(data: &DataDir, name: &str, force: bool) -> anyhow::Result<i32> {
    // Fail before the question.
    templates::find(data, name)?;
    if !force && !confirm(name) {
        cli::error!("{}", messages::cli::ABORTED);
        return Ok(0);
    }
    templates::remove(data, name)?;
    oci::gc_sweep(data);
    cli::log!("{}", messages::cli::template_removed(name));
    Ok(0)
}

/// Ask to confirm the removal of the template `name`.
/// Returns:
///   True if confirmed. Abort, Esc, no terminal or a failed prompt is "no".
fn confirm(name: &str) -> bool {
    let title = messages::cli::remove_template_title(name);
    matches!(
        choose::confirm(&title, &[], messages::cli::REMOVE_SANDBOX_YES),
        Ok(true)
    )
}

/// Return the allocated size of the disk of `template`, if it has a disk.
fn disk_used(template: &Template) -> Option<u64> {
    file_meta(template, DISK_FILE).map(|m| m.blocks() * 512)
}

/// Return the metadata of the file `name` of `template`, if it exists.
fn file_meta(template: &Template, name: &str) -> Option<Metadata> {
    template.dir.join(name).ok()?.metadata().ok()
}

/// Return the pack names of `template`, separated with commas.
fn pack_names(template: &Template) -> String {
    template
        .config
        .packs
        .iter()
        .map(|p| p.name.as_str())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Return the image name `image` with a short digest (12 hex digits).
fn short_image(image: String) -> String {
    match image.rsplit_once('@') {
        Some((name, digest)) => {
            let hex = digest.split_once(':').map_or(digest, |(_, hex)| hex);
            format!("{name}@{}", &hex[..hex.len().min(12)])
        }
        None => image,
    }
}
