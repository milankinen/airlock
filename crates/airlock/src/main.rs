//! `airlock` — host-side CLI for the airlock VM sandbox.

mod assets;
mod cache;
pub(crate) mod cli;
mod cli_server;
mod config;
mod constants;

pub(crate) mod network;
mod oci;
mod project;
mod rpc;
mod terminal;
mod util;
mod vm;

use clap::{CommandFactory, FromArgMatches};
use cli::{Cli, Command};

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Split argv at "--" before clap sees it
    let raw_args: Vec<String> = std::env::args().collect();
    let (airlock_args, extra_args) = split_at_separator(&raw_args);

    let matches = Cli::command()
        .version(cli::version_string().leak() as &str)
        .after_help(cli::platform_status())
        .get_matches_from(&airlock_args);
    let parsed = Cli::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    cli::initialize(&parsed.global);

    let exit_code = match parsed.command {
        Command::Up {
            path,
            log_level,
            project_cwd,
            login,
        } => cli::cmd_up::run(path, log_level, extra_args, project_cwd, login).await,
        Command::Exec {
            cmd,
            args,
            cwd,
            env,
            login,
        } => {
            if !extra_args.is_empty() {
                cli::error!("'--' args are not supported with 'exec'");
                std::process::exit(2);
            }
            cli::cmd_exec::run(cmd, args, cwd, env, login).await
        }
        Command::Info { path } => {
            if !extra_args.is_empty() {
                cli::error!("'--' args are only supported with 'up'");
                std::process::exit(2);
            }
            cli::cmd_info::run(path.as_deref())
        }
        Command::List => {
            if !extra_args.is_empty() {
                cli::error!("'--' args are only supported with 'up'");
                std::process::exit(2);
            }
            cli::cmd_list::run()
        }
        Command::Down { path, force } => {
            if !extra_args.is_empty() {
                cli::error!("'--' args are only supported with 'up'");
                std::process::exit(2);
            }
            cli::cmd_down::run(path.as_deref(), force)
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
