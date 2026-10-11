//! The `airlock secrets` command.
//!
//! Lists, adds and removes user secrets in the configured vault.

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand};

use crate::context::Context;
use crate::vault::{Vault, VaultStorageType, ui, validate_secret_name};
use crate::{cli, messages};

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
///   Process exit code: 0 on success or abort, 1 on error.
pub fn main(args: SecretArgs, context: &Context) -> i32 {
    let vault = &context.vault;
    if vault.storage_type() == VaultStorageType::Disabled {
        cli::error!("{}", ui::disabled_message());
        return 1;
    }
    match run(args, vault) {
        Ok(()) => 0,
        Err(e) if e.is::<ui::Aborted>() => {
            cli::error!("{}", messages::cli::ABORTED);
            0
        }
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
    let mut items = vault.list_secrets().context(messages::cli::READ_VAULT)?;
    items.sort_by(|a, b| a.name.cmp(&b.name));
    if items.is_empty() {
        cli::log!("{}", messages::cli::NO_SECRETS);
        return Ok(());
    }
    let name_w = items
        .iter()
        .map(|m| m.name.chars().count())
        .max()
        .unwrap_or(4)
        .max(messages::cli::SECRETS_COL_NAME.len());
    let value_w = items
        .iter()
        .map(|m| m.preview.chars().count())
        .max()
        .unwrap_or(5)
        .max(messages::cli::SECRETS_COL_VALUE.len());
    println!(
        "{:<name_w$}  {:<value_w$}  {}",
        messages::cli::SECRETS_COL_NAME,
        messages::cli::SECRETS_COL_VALUE,
        messages::cli::SECRETS_COL_SAVED_AT
    );
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
        bail!(messages::vault::SECRET_VALUE_EMPTY);
    }
    vault
        .set_secret(name, &value)
        .context(messages::cli::WRITE_VAULT)?;
    cli::log!("{} {}", cli::check(), messages::cli::secret_stored(name));
    Ok(())
}

fn remove(vault: &Vault, name: &str) -> anyhow::Result<()> {
    if vault
        .remove_secret(name)
        .context(messages::cli::WRITE_VAULT)?
    {
        cli::log!("{} {}", cli::check(), messages::cli::secret_removed(name));
    } else {
        cli::log!("{}", messages::cli::no_secret_named(name));
    }
    Ok(())
}
