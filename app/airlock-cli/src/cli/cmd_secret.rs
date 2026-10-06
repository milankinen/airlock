//! `airlock secrets` — manage user secrets stored in the configured
//! vault backend.
//!
//! Three subcommands: `list`, `add`, `remove`. Storage goes through
//! the single `Vault` (one JSON blob); see `crate::vault`.
//!
//! `add` never takes the value on the command line — interactive
//! prompt or `--stdin`. This is a hard rule: argv values leak via
//! shell history and `ps`.

use anyhow::{Context as _, bail};
use clap::{Args, Subcommand};

use crate::cli;
use crate::context::Context;
use crate::vault::{Vault, VaultStorageType, ui, validate_secret_name};

#[derive(Args, Debug)]
pub struct SecretArgs {
    #[command(subcommand)]
    cmd: SecretCmd,
}

#[derive(Subcommand, Debug)]
enum SecretCmd {
    /// List all stored secrets. Shows names, timestamps, and a masked
    /// `****`-prefixed preview of the last few value chars (for
    /// disambiguation only — the full value is never printed).
    #[command(alias = "ls")]
    List,
    /// Add or overwrite a secret. Prompts interactively for the
    /// value with echo suppressed. Use `--stdin` to pipe the value.
    Add {
        /// Secret name — must match `[A-Z_][A-Z0-9_]*` so it can be
        /// referenced via `${NAME}` in config.
        name: String,
        /// Read the value from stdin instead of prompting. Reads
        /// until EOF; trims a single trailing `\n`. Useful for
        /// piping secrets from scripts.
        #[arg(long)]
        stdin: bool,
        /// Skip the plaintext-vault confirmation. Has no effect on
        /// encrypted/keyring vaults.
        #[arg(short = 'y', long)]
        yes: bool,
    },
    /// Remove a secret.
    #[command(alias = "rm")]
    Remove {
        /// Secret name.
        name: String,
    },
}

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
