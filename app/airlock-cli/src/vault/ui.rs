//! Terminal interaction for `airlock secrets add`.
//!
//! Tells the user when the vault is disabled, asks for confirmation before
//! airlock stores a secret without encryption, and reads the secret value
//! from a prompt or stdin.

use std::io::Read;

use anyhow::{Context, bail};

use super::{Vault, VaultStorageType};
use crate::cli;
use crate::cli::prompt::fields::{Field, Fields};
use crate::cli::prompt::yes_no::YesNo;
use crate::settings::Settings;

/// Error message for commands that must store a secret while the vault is
/// disabled. Tells how to enable the vault.
pub fn disabled_message() -> String {
    format!(
        "the airlock vault is disabled. Enable it to store user \
         secrets and registry credentials.\n\n\
         Edit {} and set:\n\n  \
         vault.storage = \"keyring\"         # system keychain / Secret Service (default), or\n  \
         vault.storage = \"encrypted-file\"  # passphrase-encrypted, or\n  \
         vault.storage = \"file\"            # plain 0600 JSON\n",
        Settings::expected_path().display()
    )
}

/// Make sure that the vault can store a secret.
/// Args:
///  - `vault`: Vault to check
///  - `assume_yes`: Do not ask the user to confirm a plaintext vault
///
/// Returns:
///   Error if the vault is disabled, or if the vault is plaintext (`file`)
///   and the user does not confirm it.
pub fn ensure_writable(vault: &Vault, assume_yes: bool) -> anyhow::Result<()> {
    match vault.storage_type() {
        VaultStorageType::Disabled => bail!("{}", disabled_message()),
        VaultStorageType::File if !assume_yes => confirm_plaintext_vault(),
        _ => Ok(()),
    }
}

/// Warn the user that the `file` backend stores secrets as cleartext JSON,
/// and ask if they want to continue. Shows one time per `secret add`.
///
/// The warning shows the two better backends, so the user knows what to
/// do. A non-interactive shell fails. Scripts can use `--yes`.
fn confirm_plaintext_vault() -> anyhow::Result<()> {
    let msg = "\
Your vault backend is \"file\" — secrets will be written as plaintext
JSON to ~/.airlock/vault.default.json (mode 0600). Anyone with read access to
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
    let question = YesNo {
        question: "Proceed with the plaintext vault?",
        default: false,
    };
    let ok = question.ask().context("confirm plaintext vault")?;
    if ok != Some(true) {
        bail!("cancelled");
    }
    Ok(())
}

/// Read a secret value from stdin.
/// Returns:
///   Value without one trailing newline. Error if stdin is a TTY: then
///   the user would type the secret into the terminal, which shows it.
pub fn read_from_stdin() -> anyhow::Result<String> {
    // SAFETY: `libc::isatty` on fd 0 is side-effect-free.
    let is_tty = unsafe { libc::isatty(0) } == 1;
    if is_tty {
        bail!("--stdin requires piped input; omit it to prompt interactively");
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context("read secret from stdin")?;
    // Remove exactly one trailing newline, so `echo $FOO | ...` gives the
    // original value.
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    Ok(buf)
}

/// Ask the user one time for a secret value. The input is masked.
/// Args:
///  - `label`: Label of the input row
///
/// Returns:
///   Text that the user typed. Error if the terminal is not interactive
///   or the user pushes Esc.
pub fn read_from_prompt(label: &str) -> anyhow::Result<String> {
    if !cli::is_interactive() {
        bail!("no TTY available — use `--stdin` to pipe the value in");
    }
    let form = Fields {
        title: None,
        rows: &[Field {
            label,
            secret: true,
        }],
        keys: "enter save · esc cancel",
    };
    let texts = form.ask(|_| Ok(())).context("read secret value")?;
    let Some(mut texts) = texts else {
        bail!("cancelled");
    };
    Ok(texts.swap_remove(0))
}
