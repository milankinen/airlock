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
mod runtime;
mod settings;
mod util;
mod vault;
mod vm;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};

use crate::cli::{cmd_exec, cmd_rm, cmd_secret, cmd_show, cmd_start};
use crate::settings::Settings;
use crate::vault::Vault;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    // Split argv at "--" before clap sees it
    let raw_args: Vec<String> = std::env::args().collect();
    let (airlock_args, extra_args) = split_at_separator(&raw_args);

    let matches = Program::command()
        .version(cli::version_string().leak() as &str)
        .after_help(cli::platform_status())
        .get_matches_from(&airlock_args);
    let parsed = Program::from_arg_matches(&matches).unwrap_or_else(|e| e.exit());
    cli::initialize(parsed.global.quiet);

    // Application-wide settings from `~/.airlock/settings.*`. Absent
    // file → defaults. A malformed file fails loudly so the user
    // doesn't silently fall back to defaults.
    let settings = match Settings::load() {
        Ok(s) => s,
        Err(e) => {
            cli::error!("{e:#}");
            std::process::exit(1);
        }
    };

    // One Vault per process, threaded into every subcommand. The
    // backend is selected by `settings.vault`; for `disabled` the
    // vault is an inert no-op so no callee has to special-case it.
    // `Vault` is cheaply cloneable (Arc inside) and passed by value.
    let vault = Vault::for_storage_type(settings.vault.storage);

    let exit_code = match parsed.command {
        Command::Start(args) => cli::cmd_start::main(args, extra_args, vault).await,
        Command::Exec(args) => {
            if !extra_args.is_empty() {
                cli::error!("'--' args are not supported with 'exec'");
                std::process::exit(2);
            }
            cli::cmd_exec::main(args).await
        }
        Command::Show(ref args) => {
            if !extra_args.is_empty() {
                cli::error!("'--' args are only supported with 'start'");
                std::process::exit(2);
            }
            cli::cmd_show::main(args, vault)
        }
        Command::Remove(ref args) => {
            if !extra_args.is_empty() {
                cli::error!("'--' args are only supported with 'start'");
                std::process::exit(2);
            }
            cli::cmd_rm::main(args, vault)
        }
        Command::Secrets(args) => {
            if !extra_args.is_empty() {
                cli::error!("'--' args are only supported with 'start'");
                std::process::exit(2);
            }
            cli::cmd_secret::main(args, &vault, &settings)
        }
    };

    std::process::exit(exit_code);
}

/// Top-level CLI definition. Clap derives argument parsing from this struct.
#[derive(Parser)]
#[command(
    name = "airlock",
    version,
    about = "Lightweight VM sandbox for running untrusted code"
)]
struct Program {
    #[command(flatten)]
    pub global: GlobalArgs,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Args, Debug)]
struct GlobalArgs {
    /// Suppress airlock cli output
    #[arg(short, long, default_value_t = false)]
    quiet: bool,
}

#[derive(Subcommand)]
enum Command {
    /// Start the sandbox VM for the current project directory
    Start(cmd_start::StartArgs),
    /// Remove the current project data
    #[command(alias = "rm")]
    Remove(cmd_rm::RmArgs),
    /// Execute a command inside the running sandbox VM
    #[command(alias = "x")]
    Exec(cmd_exec::ExecArgs),
    /// Show the current project info
    Show(cmd_show::ShowArgs),
    /// Manage secrets stored in the system keyring
    #[command(alias = "secret")]
    Secrets(cmd_secret::SecretArgs),
}

/// Split argv at "--". Returns (args before --, args after --).
fn split_at_separator(args: &[String]) -> (Vec<String>, Vec<String>) {
    if let Some(pos) = args.iter().position(|a| a == "--") {
        (args[..pos].to_vec(), args[pos + 1..].to_vec())
    } else {
        (args.to_vec(), vec![])
    }
}
