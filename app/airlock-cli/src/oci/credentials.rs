//! Registry credentials.
//!
//! Loads and saves the credentials of private image registries, and asks the
//! user for them when necessary. The credentials are kept in the secret
//! vault.

use oci_client::secrets::RegistryAuth;

use crate::cli::prompt::fields::{Field, Fields, Invalid};
use crate::vault::{RegistryCreds, Vault};

/// Conversion of stored credentials to `RegistryAuth`. Callers can write
/// `creds.to_auth()` instead of building `RegistryAuth::Basic` from the
/// `username` and `password` fields each time.
pub trait ToRegistryAuth {
    /// Return the credentials as `RegistryAuth`.
    fn to_auth(&self) -> RegistryAuth;
}

impl ToRegistryAuth for RegistryCreds {
    fn to_auth(&self) -> RegistryAuth {
        RegistryAuth::Basic(self.username.clone(), self.password.clone())
    }
}

/// Load the stored credentials of a registry host from the vault.
/// Returns:
///   The credentials, or `None` if there are none or the vault fails.
pub fn load(vault: &Vault, registry_host: &str) -> Option<RegistryCreds> {
    match vault.get_registry(registry_host) {
        Ok(found) => found,
        Err(e) => {
            // Log a vault failure and treat it as "no saved credentials".
            // The caller then uses anonymous auth and asks the user on 401.
            // Thus a broken or locked vault cannot stop an image pull that
            // did not need authentication.
            tracing::debug!("vault unavailable while loading registry creds: {e:#}");
            None
        }
    }
}

/// Save the credentials of a registry host into the vault.
pub fn save(vault: &Vault, registry_host: &str, creds: &RegistryCreds) -> anyhow::Result<()> {
    vault.set_registry(registry_host, creds)
}

/// Ask the user for the credentials of a registry host. The username and
/// the password must not be empty.
/// Returns:
///   The credentials. Error if the process has no TTY or the user presses
///   Esc.
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
