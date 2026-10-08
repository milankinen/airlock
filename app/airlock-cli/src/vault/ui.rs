//! Terminal interaction for storing secrets with `airlock secrets add`:
//! the disabled-vault message, the plaintext-vault confirmation and
//! reading a value from a prompt or stdin.

use std::io::Read;

use anyhow::{Context, bail};

use super::{Vault, VaultStorageType};
use crate::cli;
use crate::cli::prompt::fields::{Field, Fields};
use crate::cli::prompt::yes_no::YesNo;
use crate::settings::Settings;

/// The error for commands that must store a secret while the vault is
/// disabled: how to enable it.
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

/// Check that a secret can be stored: the vault is not disabled, and a
/// plaintext (`file`) vault is confirmed by the user unless `assume_yes`.
pub fn ensure_writable(vault: &Vault, assume_yes: bool) -> anyhow::Result<()> {
    match vault.storage_type() {
        VaultStorageType::Disabled => bail!("{}", disabled_message()),
        VaultStorageType::File if !assume_yes => confirm_plaintext_vault(),
        _ => Ok(()),
    }
}

/// Warn the user — once per `secret add` — that the `file` backend
/// stores secrets as cleartext JSON, and ask whether to proceed. Point
/// at the two better options so the warning carries actionable advice
/// instead of just friction. A non-interactive shell fails closed; the
/// caller can pass `--yes` when scripting.
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

/// Read value from stdin. Errors if stdin is a TTY (prevents users
/// from running `airlock secrets add FOO --stdin` and then typing into
/// their terminal, which would echo the secret).
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
    // Trim exactly one trailing newline so `echo $FOO | ...` round-trips.
    if buf.ends_with('\n') {
        buf.pop();
        if buf.ends_with('\r') {
            buf.pop();
        }
    }
    Ok(buf)
}

/// Prompt once for a value (the row `label`), masked; accept whatever the
/// user types. Abort with an error if the TTY isn't interactive, and on
/// Esc.
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
