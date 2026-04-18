//! Registry credential handling. Storage lives in the system-keyring-
//! backed `crate::vault::Vault`; this module is just the OCI-specific
//! adapter (prompting and `RegistryAuth` conversion).

use dialoguer::theme::ColorfulTheme;
use dialoguer::{Input, Password};
use oci_client::secrets::RegistryAuth;

use crate::vault::{RegistryCreds, Vault};

/// Convenience adapter so callers can write `creds.to_auth()` instead
/// of threading the `username` and `password` fields through every
/// `RegistryAuth::Basic` construction.
pub trait ToRegistryAuth {
    fn to_auth(&self) -> RegistryAuth;
}

impl ToRegistryAuth for RegistryCreds {
    fn to_auth(&self) -> RegistryAuth {
        RegistryAuth::Basic(self.username.clone(), self.password.clone())
    }
}

/// Load stored credentials for `registry_host` from the vault. A
/// keyring failure is logged and treated as "no saved creds" — the
/// caller falls back to anonymous auth and prompts on 401, so a
/// broken/locked keyring can't stop an image pull that didn't need
/// authentication in the first place.
pub fn load(vault: &Vault, registry_host: &str) -> Option<RegistryCreds> {
    match vault.get_registry(registry_host) {
        Ok(found) => found,
        Err(e) => {
            tracing::debug!("vault unavailable while loading registry creds: {e:#}");
            None
        }
    }
}

/// Save credentials for `registry_host` into the vault.
pub fn save(vault: &Vault, registry_host: &str, creds: &RegistryCreds) -> anyhow::Result<()> {
    vault.set_registry(registry_host, creds)
}

/// Prompt the user interactively for registry credentials. Errors if
/// the process isn't attached to a TTY — the CLI caller can then fall
/// back to treating the registry as anonymous.
pub fn prompt(registry_host: &str) -> anyhow::Result<RegistryCreds> {
    if !crate::cli::is_interactive() {
        anyhow::bail!("registry {registry_host} requires authentication");
    }
    let theme = ColorfulTheme::default();
    let term = console::Term::stderr();
    let username: String = Input::with_theme(&theme)
        .with_prompt(format!("Username for {registry_host}"))
        .interact_on(&term)?;
    let password = Password::with_theme(&theme)
        .with_prompt(format!("Password for {registry_host}"))
        .interact_on(&term)?;
    Ok(RegistryCreds { username, password })
}
