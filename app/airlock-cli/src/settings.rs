//! Application-wide user settings loaded from `~/.airlock/settings.*`.
//!
//! Resolved once at `main` and threaded into subcommands. Shares the
//! smart-config pipeline with the project-level `airlock.toml` loader
//! (`crate::config::load_config`): same TOML/JSON/YAML auto-detect,
//! same parse-error formatting. Missing file → defaults, which keeps
//! `airlock` usable with zero configuration.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, DeserializeConfig, Json};

use crate::config::de::format_error;
use crate::config::load_config::{EXTENSIONS, parse_file};
use crate::vault::VaultStorageType;

/// All user-tunable settings. Add fields here; the default for each
/// field must keep `airlock` usable without a settings file.
#[derive(Clone, Debug, Default, DescribeConfig, DeserializeConfig)]
pub struct Settings {
    /// Vault configuration. Nested under `[vault]` so future vault-related
    /// knobs (passphrase caching policy, custom storage path, ...) fit
    /// alongside `storage` without polluting the top-level namespace.
    #[config(nest)]
    pub vault: VaultSettings,
}

/// Settings under the `[vault]` table.
#[derive(Clone, Debug, Default, DescribeConfig, DeserializeConfig)]
pub struct VaultSettings {
    /// Which backend stores user secrets and registry credentials.
    /// Defaults to `file` — a mode-0600 JSON file under `~/.airlock/`.
    /// Switch to `encrypted-file` for AEAD-at-rest, `keyring` for the
    /// system keychain, or `disabled` to turn the vault off entirely.
    #[config(default)]
    pub storage: VaultStorageType,
}

impl Settings {
    pub fn dir() -> Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory missing"))?;
        Ok(home.join(".airlock"))
    }

    /// Human-readable path where the TOML settings file should live.
    /// Used in error messages that ask the user to create/edit it.
    pub fn expected_path() -> PathBuf {
        PathBuf::from("~/.airlock/settings.toml")
    }

    /// Load settings from the first matching `~/.airlock/settings.*`
    /// file. Missing file → defaults. Parse errors bubble up so the
    /// user notices a malformed file instead of silently getting
    /// defaults.
    pub fn load() -> Result<Self> {
        Self::load_from(&Self::dir()?)
    }

    fn load_from(dir: &Path) -> Result<Self> {
        // Same extension ordering (TOML → JSON → YAML) as the project
        // config loader. TOML wins if multiple files exist, so a stray
        // `settings.json` can't shadow the user's primary `settings.toml`.
        for ext in EXTENSIONS {
            let path = dir.join(format!("settings.{ext}"));
            if !path.exists() {
                continue;
            }
            let content = std::fs::read_to_string(&path)
                .with_context(|| format!("read settings file {}", path.display()))?;
            let value = parse_file(&path, &content)?;
            return parse_settings(value)
                .with_context(|| format!("load settings file {}", path.display()));
        }
        Ok(Self::default())
    }
}

/// Feed the parsed file (as a JSON object) through smart-config using
/// the same pipeline as the project config loader. Unknown fields and
/// type mismatches surface here as structured parse errors.
fn parse_settings(value: serde_json::Value) -> Result<Settings> {
    let serde_json::Value::Object(map) = value else {
        bail!("settings must be a table");
    };
    let schema = ConfigSchema::new(&Settings::DESCRIPTION, "");
    let source = Json::new("settings", map);
    let repo = ConfigRepository::new(&schema).with(source);
    let parser = repo.single::<Settings>()?;
    parser
        .parse()
        .map_err(|errors| anyhow!(format_error("invalid settings", errors)))
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    fn fresh_dir() -> PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let id = N.fetch_add(1, Ordering::Relaxed);
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let dir = std::env::temp_dir().join(format!("airlock-settings-test-{ts}-{id}"));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn missing_dir_yields_defaults() {
        let base = fresh_dir();
        let missing = base.join("nope");
        let s = Settings::load_from(&missing).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::EncryptedFile);
    }

    #[test]
    fn toml_roundtrip() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.toml"), "vault.storage = \"file\"\n").unwrap();
        let s = Settings::load_from(&dir).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::File);
    }

    #[test]
    fn json_roundtrip() {
        let dir = fresh_dir();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"vault": {"storage": "keyring"}}"#,
        )
        .unwrap();
        let s = Settings::load_from(&dir).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::Keyring);
    }

    #[test]
    fn yaml_roundtrip() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.yml"), "vault:\n  storage: disabled\n").unwrap();
        let s = Settings::load_from(&dir).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::Disabled);
    }

    /// TOML wins when multiple candidates exist — stable ordering
    /// matters so a stray `settings.json` doesn't shadow the user's
    /// primary TOML file.
    #[test]
    fn toml_wins_over_json() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.toml"), "vault.storage = \"keyring\"\n").unwrap();
        std::fs::write(
            dir.join("settings.json"),
            r#"{"vault": {"storage": "disabled"}}"#,
        )
        .unwrap();
        let s = Settings::load_from(&dir).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::Keyring);
    }

    #[test]
    fn malformed_file_errors() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.toml"), "not valid = toml =").unwrap();
        assert!(Settings::load_from(&dir).is_err());
    }

    /// Unknown enum variants must not silently degrade to the default —
    /// a typo in `vault.storage = "file"` would otherwise be
    /// indistinguishable from "user didn't set it".
    #[test]
    fn bad_vault_value_errors() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.toml"), "vault.storage = \"typo\"\n").unwrap();
        assert!(Settings::load_from(&dir).is_err());
    }
}
