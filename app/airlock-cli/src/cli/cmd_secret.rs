//! The `airlock secrets` command.
//!
//! Lists, adds and removes user secrets in the configured vault.

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand};

use crate::cli;
use crate::context::Context;
use crate::vault::{Vault, VaultStorageType, ui, validate_secret_name};

/// CLI arguments for `airlock secrets`.
#[derive(Args, Debug)]
pub struct SecretArgs {
    #[command(subcommand)]
    cmd: SecretCmd,
}

/// Subcommands of `airlock secrets`.
#[derive(Subcommand, Debug)]
enum SecretCmd {
    /// List secrets: names, save times, masked value previews (never full values)
    #[command(alias = "ls")]
    List,
    /// Add or replace a secret. Asks for the value without echo
    // The value never comes from the command line, only from the prompt or
    // `--stdin`. This is a hard rule: argv values leak through shell history
    // and `ps`.
    Add {
        /// Secret name, must match `[A-Z_][A-Z0-9_]*` (use as `${NAME}` in config)
        name: String,
        /// Read the value from stdin until EOF (removes one trailing newline)
        #[arg(long)]
        stdin: bool,
        /// Do not ask to confirm a plaintext vault (no effect on other vaults)
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Remove a secret.
    #[command(alias = "rm")]
    Remove {
        /// Secret name
        name: String,
    },
}

/// Entry point for `airlock secrets`.
/// Returns:
///   Process exit code: 0 on success, 1 on error.
pub fn main(args: SecretArgs, context: &Context) -> i32 {
    let vault = &context.vault;
    if vault.storage_type() == VaultStorageType::Disabled {
        cli::error!("{}", ui::disabled_message());
        return 1;
    }
    match run(args, vault) {
        Ok(()) => 0,
        Err(e) => {
            cli::error!("{e:#}");
            1
        }
    }
}

fn run(args: SecretArgs, vault: &Vault) -> anyhow::Result<()> {
    match args.cmd {
        SecretCmd::List => list(vault),
        SecretCmd::Add { name, stdin, yes } => add(vault, &name, stdin, yes),
        SecretCmd::Remove { name } => remove(vault, &name),
    }
}

// ── Subcommands ──────────────────────────────────────────────────────────────

fn list(vault: &Vault) -> anyhow::Result<()> {
    let mut items = vault.list_secrets().context("read airlock vault")?;
    items.sort_by(|a, b| a.name.cmp(&b.name));
    if items.is_empty() {
        cli::log!("No secrets stored. Add one with `airlock secrets add <NAME>`.");
        return Ok(());
    }
    let name_w = items
        .iter()
        .map(|m| m.name.chars().count())
        .max()
        .unwrap_or(4)
        .max("NAME".len());
    let value_w = items
        .iter()
        .map(|m| m.preview.chars().count())
        .max()
        .unwrap_or(5)
        .max("VALUE".len());
    println!("{:<name_w$}  {:<value_w$}  SAVED AT", "NAME", "VALUE");
    for item in items {
        println!(
            "{:<name_w$}  {:<value_w$}  {}",
            item.name,
            item.preview,
            cli::format_local_time(item.saved_at)
        );
    }
    Ok(())
}

fn add(vault: &Vault, name: &str, use_stdin: bool, yes: bool) -> anyhow::Result<()> {
    validate_secret_name(name)?;
    ui::ensure_writable(vault, yes)?;
    let value = if use_stdin {
        ui::read_from_stdin()?
    } else {
        ui::read_from_prompt("value")?
    };
    if value.is_empty() {
        bail!("secret value must not be empty");
    }
    vault
        .set_secret(name, &value)
        .context("write airlock vault")?;
    cli::log!("{} stored secret {name}", cli::check());
    Ok(())
}

fn remove(vault: &Vault, name: &str) -> anyhow::Result<()> {
    if vault.remove_secret(name).context("write airlock vault")? {
        cli::log!("{} removed secret {name}", cli::check());
    } else {
        cli::log!("No secret named {name}");
    }
    Ok(())
}
