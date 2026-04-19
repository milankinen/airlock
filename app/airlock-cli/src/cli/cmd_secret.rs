//! `airlock secret` — manage user secrets stored in the configured
//! vault backend.
//!
//! Three subcommands: `ls`, `add`, `rm`. Storage goes through the
//! single `Vault` (one JSON blob); see `crate::vault`.
//!
//! `add` never takes the value on the command line — interactive
//! prompt or `--stdin`. This is a hard rule: argv values leak via
//! shell history and `ps`.

use std::io::Read;

use anyhow::{Context, bail};
use clap::{Args, Subcommand};
use dialoguer::theme::ColorfulTheme;
use dialoguer::{Confirm, Password};

use crate::cli;
use crate::settings::Settings;
use crate::vault::{Vault, VaultStorageType, validate_secret_name};

#[derive(Args, Debug)]
pub struct SecretArgs {
    #[command(subcommand)]
    cmd: SecretCmd,
}

#[derive(Subcommand, Debug)]
enum SecretCmd {
    /// List all stored secrets (names + timestamps only; values are
    /// never shown).
    Ls,
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
    Rm {
        /// Secret name.
        name: String,
    },
}

pub fn main(args: SecretArgs, vault: &Vault, _settings: &Settings) -> i32 {
    if vault.storage_type() == VaultStorageType::Disabled {
        cli::error!(
            "the airlock vault is disabled. Enable it to store user \
             secrets and registry credentials.\n\n\
             Edit {} and set:\n\n  \
             vault.storage = \"file\"            # plain 0600 JSON, or\n  \
             vault.storage = \"encrypted-file\"  # passphrase-encrypted, or\n  \
             vault.storage = \"keyring\"         # system keychain / Secret Service\n",
            Settings::expected_path().display()
        );
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
        SecretCmd::Ls => list(vault),
        SecretCmd::Add { name, stdin, yes } => add(vault, &name, stdin, yes),
        SecretCmd::Rm { name } => remove(vault, &name),
    }
}

// ── Subcommands ──────────────────────────────────────────────────────────────

fn list(vault: &Vault) -> anyhow::Result<()> {
    let items = vault.list_secrets().context("read airlock vault")?;
    if items.is_empty() {
        cli::log!("No secrets stored. Add one with `airlock secret add <NAME>`.");
        return Ok(());
    }
    let name_w = items
        .iter()
        .map(|m| m.name.chars().count())
        .max()
        .unwrap_or(4)
        .max("NAME".len());
    println!("{:<name_w$}  SAVED AT", "NAME");
    for item in items {
        println!(
            "{:<name_w$}  {}",
            item.name,
            format_local_time(item.saved_at)
        );
    }
    Ok(())
}

fn add(vault: &Vault, name: &str, use_stdin: bool, yes: bool) -> anyhow::Result<()> {
    validate_secret_name(name)?;
    if vault.storage_type() == VaultStorageType::File && !yes {
        confirm_plaintext_vault()?;
    }
    let value = if use_stdin {
        read_from_stdin()?
    } else {
        read_from_prompt()?
    };
    if value.is_empty() {
        bail!("secret value must not be empty");
    }
    vault
        .set_secret(name, &value)
        .context("write airlock vault")?;
    cli::log!("  {} stored secret {name}", cli::check());
    Ok(())
}

fn remove(vault: &Vault, name: &str) -> anyhow::Result<()> {
    if vault.remove_secret(name).context("write airlock vault")? {
        cli::log!("  {} removed secret {name}", cli::check());
    } else {
        cli::log!("No secret named {name}");
    }
    Ok(())
}

// ── Plaintext-vault confirmation ─────────────────────────────────────────────

/// Warn the user — once per `secret add` — that the `file` backend
/// stores secrets as cleartext JSON, and ask whether to proceed. Point
/// at the two better options so the warning carries actionable advice
/// instead of just friction. A non-interactive shell fails closed; the
/// caller can pass `--yes` when scripting.
fn confirm_plaintext_vault() -> anyhow::Result<()> {
    let msg = "\
Your vault backend is \"file\" — secrets will be written as plaintext
JSON to ~/.airlock/vault.json (mode 0600). Anyone with read access to
that file can recover them. For stronger at-rest protection, set one of
these in ~/.airlock/settings.toml:

# RECOMMENDED: Keychain (macOS) / Secret Service (Linux)
vault.storage = \"keyring\"
# AEAD-encrypted, passphrase on first use (AIRLOCK_VAULT_PASSPHRASE for non-interactive)
vault.storage = \"encrypted-file\"
";
    println!("{}", cli::yellow(msg));
    if !cli::is_interactive() {
        bail!(
            "vault is \"file\" (plaintext). Re-run with --yes to confirm, or switch to \
             \"encrypted-file\" / \"keyring\" in ~/.airlock/settings.toml."
        );
    }
    let ok = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Proceed with the plaintext vault?")
        .default(false)
        .interact()
        .context("confirm plaintext vault")?;
    if !ok {
        bail!("cancelled");
    }
    Ok(())
}

// ── Value input ──────────────────────────────────────────────────────────────

/// Read value from stdin. Errors if stdin is a TTY (prevents users
/// from running `airlock secret add FOO --stdin` and then typing into
/// their terminal, which would echo the secret).
fn read_from_stdin() -> anyhow::Result<String> {
    // SAFETY: `libc::isatty` on fd 0 is side-effect-free.
    let is_tty = unsafe { libc::isatty(0) } == 1;
    if is_tty {
        bail!("--stdin requires piped input; omit it to prompt interactively");
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("read secret from stdin")?;
    // Trim exactly one trailing newline so `echo $FOO | ...` round-trips.
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    Ok(buf)
}

/// Prompt once for the value with echo suppressed; accept whatever
/// the user types. Abort with an error if the TTY isn't interactive.
fn read_from_prompt() -> anyhow::Result<String> {
    if !cli::is_interactive() {
        bail!("no TTY available — use `--stdin` to pipe the value in");
    }
    let theme = ColorfulTheme::default();
    let term = console::Term::stderr();
    Password::with_theme(&theme)
        .with_prompt("Value")
        .allow_empty_password(true)
        .interact_on(&term)
        .context("read secret value")
}

// ── Formatting ───────────────────────────────────────────────────────────────

/// Format a `SystemTime` as local `YYYY-MM-DD HH:MM:SS` via
/// `libc::localtime_r`. Matches the style used in the TUI's network
/// log — but kept inline here so the CLI doesn't pull in the TUI crate
/// for one helper.
fn format_local_time(t: std::time::SystemTime) -> String {
    let secs = t
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let tt = secs as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    let ok = unsafe { !libc::localtime_r(&raw const tt, &raw mut tm).is_null() };
    if !ok {
        return "----/--/-- --:--:--".to_string();
    }
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        tm.tm_year + 1900,
        tm.tm_mon + 1,
        tm.tm_mday,
        tm.tm_hour,
        tm.tm_min,
        tm.tm_sec,
    )
}
