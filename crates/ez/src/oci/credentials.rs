//! Registry credential storage and interactive prompting.
//!
//! macOS: credentials stored in the system Keychain (Security framework).
//! Linux/other: credentials stored in `~/.ezpez/registry-credentials.json`
//! with 0600 permissions.

use dialoguer::{Input, Password};
use oci_client::secrets::RegistryAuth;
use serde::{Deserialize, Serialize};

#[derive(Clone, Serialize, Deserialize)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    pub fn to_auth(&self) -> RegistryAuth {
        RegistryAuth::Basic(self.username.clone(), self.password.clone())
    }
}

/// Load stored credentials for `registry_host`. Returns `None` if not found.
pub fn load(registry_host: &str) -> Option<Credentials> {
    load_impl(registry_host)
}

/// Save credentials for `registry_host`.
pub fn save(registry_host: &str, creds: &Credentials) -> anyhow::Result<()> {
    save_impl(registry_host, creds)
}

/// Prompt the user interactively for registry credentials.
/// Returns an error if not running in an interactive terminal.
pub fn prompt(registry_host: &str) -> anyhow::Result<Credentials> {
    if !crate::cli::is_interactive() {
        anyhow::bail!("registry {registry_host} requires authentication");
    }
    let term = console::Term::stderr();
    let username: String = Input::new()
        .with_prompt(format!("Username for {registry_host}"))
        .interact_on(&term)?;
    let password = Password::new()
        .with_prompt(format!("Password for {registry_host}"))
        .interact_on(&term)?;
    Ok(Credentials { username, password })
}

// ── macOS Keychain ────────────────────────────────────────────────────────────

#[cfg(target_os = "macos")]
fn load_impl(registry_host: &str) -> Option<Credentials> {
    use security_framework::passwords::get_generic_password;
    let bytes = get_generic_password("ezpez-registry", registry_host).ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(target_os = "macos")]
fn save_impl(registry_host: &str, creds: &Credentials) -> anyhow::Result<()> {
    use security_framework::passwords::set_generic_password;
    let bytes = serde_json::to_vec(creds)?;
    set_generic_password("ezpez-registry", registry_host, &bytes)
        .map_err(|e| anyhow::anyhow!("keychain error: {e}"))?;
    Ok(())
}

// ── Linux / other: JSON file ──────────────────────────────────────────────────

#[cfg(not(target_os = "macos"))]
fn load_impl(registry_host: &str) -> Option<Credentials> {
    let path = credentials_file_path().ok()?;
    let data = std::fs::read(&path).ok()?;
    let map: std::collections::HashMap<String, Credentials> = serde_json::from_slice(&data).ok()?;
    map.get(registry_host).cloned()
}

#[cfg(not(target_os = "macos"))]
fn save_impl(registry_host: &str, creds: &Credentials) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let path = credentials_file_path()?;
    let mut map: std::collections::HashMap<String, Credentials> = if path.exists() {
        let data = std::fs::read(&path)?;
        serde_json::from_slice(&data).unwrap_or_default()
    } else {
        std::collections::HashMap::new()
    };
    map.insert(registry_host.to_string(), creds.clone());
    let json = serde_json::to_string_pretty(&map)?;
    std::fs::write(&path, &json)?;
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn credentials_file_path() -> anyhow::Result<std::path::PathBuf> {
    Ok(crate::cache::cache_dir()?.join("registry-credentials.json"))
}
