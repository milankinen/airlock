//! Terminal interaction for `airlock secrets add`.
//!
//! Tells the user when the vault is disabled, asks for confirmation before
//! airlock stores a secret without encryption, and reads the secret value
//! from a prompt or stdin.

use std::io::Read;

use anyhow::{Context, bail};

use super::{Vault, VaultStorageType};
use crate::cli::prompt::choose;
use crate::cli::prompt::fields::{Field, Fields};
use crate::settings::Settings;
use crate::{cli, messages};

/// The user aborted a prompt. The command ends with exit code 0.
#[derive(Debug, thiserror::Error)]
#[error("{}", crate::messages::cli::ABORTED)]
pub struct Aborted;

/// Error message for commands that must store a secret while the vault is
/// disabled. Tells how to enable the vault.
pub fn disabled_message() -> String {
    messages::vault::vault_disabled(Settings::expected_path())
}

/// Make sure that the vault can store a secret.
/// Args:
///  - `vault`: Vault to check
///  - `assume_yes`: Do not ask the user to confirm a plaintext vault
///
/// Returns:
///   Error if the vault is disabled, or if the vault is plaintext (`file`)
///   and the user does not confirm it. [`Aborted`] if the user aborts.
pub fn ensure_writable(vault: &Vault, assume_yes: bool) -> anyhow::Result<()> {
    match vault.storage_type() {
        VaultStorageType::Disabled => bail!(disabled_message()),
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
    println!("{}", cli::yellow(messages::vault::PLAINTEXT_VAULT_WARNING));
    if !cli::is_interactive() {
        bail!(messages::vault::PLAINTEXT_VAULT_NEEDS_YES);
    }
    let ok = choose::confirm(
        messages::vault::PROCEED_PLAINTEXT_VAULT_QUESTION,
        &[],
        messages::vault::PROCEED_PLAINTEXT_VAULT_YES,
    )
    .context(messages::vault::CONFIRM_PLAINTEXT_VAULT)?;
    if !ok {
        return Err(Aborted.into());
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
        bail!(messages::vault::STDIN_NEEDS_PIPED_INPUT);
    }
    let mut buf = String::new();
    std::io::stdin()
        .read_to_string(&mut buf)
        .context(messages::vault::READ_SECRET_FROM_STDIN)?;
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
///   Text that the user typed. Error if the terminal is not interactive.
///   [`Aborted`] if the user pushes Esc.
pub fn read_from_prompt(label: &str) -> anyhow::Result<String> {
    if !cli::is_interactive() {
        bail!(messages::vault::NO_TTY_USE_STDIN);
    }
    let form = Fields {
        title: None,
        rows: &[Field {
            label,
            secret: true,
        }],
        keys: messages::vault::SECRET_KEYS,
    };
    let texts = form
        .ask(|_| Ok(()))
        .context(messages::vault::READ_SECRET_VALUE)?;
    let Some(mut texts) = texts else {
        return Err(Aborted.into());
    };
    Ok(texts.swap_remove(0))
}
