//! Registry credential handling. Storage lives in the system-keyring-
//! backed `crate::vault::Vault`; this module is just the OCI-specific
//! adapter (prompting and `RegistryAuth` conversion).

use oci_client::secrets::RegistryAuth;

use crate::cli::prompt::fields::{Field, Fields, Invalid};
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
/// back to treating the registry as anonymous — and on Esc. Both texts
/// are required.
pub fn prompt(registry_host: &str) -> anyhow::Result<RegistryCreds> {
    if !crate::cli::is_interactive() {
        anyhow::bail!("registry {registry_host} requires authentication");
    }
    let title = format!("Sign in to {registry_host}");
    let form = Fields {
        title: Some(&title),
        rows: &[
            Field {
                label: "username",
                secret: false,
            },
            Field {
                label: "password",
                secret: true,
            },
        ],
        keys: "enter next · esc cancel",
    };
    let required = |texts: &[String]| match texts.iter().position(String::is_empty) {
        Some(field) => Err(Invalid {
            field,
            message: format!("{} must not be empty", form.rows[field].label),
        }),
        None => Ok(()),
    };
    let Some(mut texts) = form.ask(required)? else {
        anyhow::bail!("sign-in to {registry_host} cancelled");
    };
    let password = texts.pop().unwrap_or_default();
    let username = texts.pop().unwrap_or_default();
    Ok(RegistryCreds { username, password })
}
