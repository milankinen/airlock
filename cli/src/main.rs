mod assets;
mod cache;
pub(crate) mod cli;
mod config;

pub(crate) mod network;
mod oci;
mod project;
mod rpc;
mod terminal;
mod vm;

use clap::{CommandFactory, FromArgMatches};
use cli::{Cli, Command, ProjectCommand};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Split argv at "--" before clap sees it
    let raw_args: Vec<String> = std::env::args().collect();
    let (ez_args, extra_args) = split_at_separator(&raw_args);

    let matches = Cli::command()
        .version(cli::version_string().leak() as &str)
        .after_help(cli::platform_status())
        .get_matches_from(&ez_args);
    let parsed = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    cli::initialize(&parsed.global);

    let exit_code = match parsed.command {
        Command::Go { log_level } => cli::cmd_go::run(log_level, extra_args).await,
        Command::Project { command } => {
            if !extra_args.is_empty() {
                cli::error!("'--' args are only supported with 'go' command");
                std::process::exit(2);
            }
            match command {
                ProjectCommand::Info { path } => cli::cmd_project_info::run(path.as_deref()),
                ProjectCommand::List => cli::cmd_project_list::run(),
                ProjectCommand::Remove { path, yes } => {
                    cli::cmd_project_remove::run(path.as_deref(), yes)
                }
            }
        }
    };

    std::process::exit(exit_code);
}

/// Split argv at "--". Returns (args before --, args after --).
fn split_at_separator(args: &[String]) -> (Vec<String>, Vec<String>) {
    if let Some(pos) = args.iter().position(|a| a == "--") {
        (args[..pos].to_vec(), args[pos + 1..].to_vec())
    } else {
        (args.to_vec(), vec![])
    }
}
