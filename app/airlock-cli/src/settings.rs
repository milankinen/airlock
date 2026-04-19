//! Application-wide user settings loaded from `~/.airlock/settings.*`.
//!
//! Resolved once at `main` and threaded into subcommands. Supports TOML
//! (preferred), JSON, and YAML so users can bring whichever format their
//! dotfiles already speak; the first matching filename wins.
//!
//! Absent file → all-default settings. That keeps `airlock` usable with
//! zero configuration — settings exist only for the opt-in features that
//! would otherwise surprise first-time users (right now: secret storage).

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

/// All user-tunable settings. Add fields here; the default for each
/// field must keep `airlock` usable without a settings file.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Settings {
    /// Enable the keyring-backed vault for user secrets *and* registry
    /// credentials. When `false`, `airlock secret` refuses to run and
    /// the vault behaves as if empty (no keyring I/O, no unlock
    /// prompts); registry auth falls back to re-prompting on 401. Off
    /// by default because the keyring unlock is surprising on first
    /// contact.
    pub vault_enabled: bool,
}

/// File names tried in order under `~/.airlock/`. TOML leads because the
/// rest of airlock's config already speaks TOML.
const CANDIDATES: &[(&str, Format)] = &[
    ("settings.toml", Format::Toml),
    ("settings.json", Format::Json),
    ("settings.yaml", Format::Yaml),
    ("settings.yml", Format::Yaml),
];

#[derive(Clone, Copy)]
enum Format {
    Toml,
    Json,
    Yaml,
}

impl Settings {
    /// Load settings from the first matching `~/.airlock/settings.*`
    /// file. Missing file → defaults. Parse errors bubble up so the
    /// user notices a malformed file instead of silently getting
    /// defaults.
    pub fn load() -> Result<Self> {
        let Some(home) = dirs::home_dir() else {
            return Ok(Self::default());
        };
        Self::load_from(&home.join(".airlock"))
    }

    fn load_from(dir: &Path) -> Result<Self> {
        for (name, fmt) in CANDIDATES {
            let path = dir.join(name);
            if !path.exists() {
                continue;
            }
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("read settings file {}", path.display()))?;
            let settings: Self = match fmt {
                Format::Toml => toml::from_str(&raw)
                    .with_context(|| format!("parse settings file {}", path.display()))?,
                Format::Json => serde_json::from_str(&raw)
                    .with_context(|| format!("parse settings file {}", path.display()))?,
                Format::Yaml => serde_yaml::from_str(&raw)
                    .with_context(|| format!("parse settings file {}", path.display()))?,
            };
            return Ok(settings);
        }
        Ok(Self::default())
    }

    /// Human-readable path where the TOML settings file should live.
    /// Used in error messages that ask the user to create/edit it.
    pub fn expected_path() -> PathBuf {
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("~"))
            .join(".airlock/settings.toml")
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicU32, Ordering};

    use super::*;

    /// Unique per-test directory under the system temp dir. Plain
    /// `env::temp_dir()` is shared, so we suffix each call with a
    /// monotonic counter + nanosecond timestamp so parallel tests
    /// don't stomp each other.
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
        assert!(!s.vault_enabled);
    }

    #[test]
    fn toml_roundtrip() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.toml"), "vault_enabled = true\n").unwrap();
        let s = Settings::load_from(&dir).unwrap();
        assert!(s.vault_enabled);
    }

    #[test]
    fn json_roundtrip() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.json"), r#"{"vault_enabled": true}"#).unwrap();
        let s = Settings::load_from(&dir).unwrap();
        assert!(s.vault_enabled);
    }

    #[test]
    fn yaml_roundtrip() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.yml"), "vault_enabled: true\n").unwrap();
        let s = Settings::load_from(&dir).unwrap();
        assert!(s.vault_enabled);
    }

    /// TOML wins when multiple candidates exist — stable ordering
    /// matters so a stray `settings.json` doesn't shadow the user's
    /// primary TOML file.
    #[test]
    fn toml_wins_over_json() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.toml"), "vault_enabled = true\n").unwrap();
        std::fs::write(dir.join("settings.json"), r#"{"vault_enabled": false}"#).unwrap();
        let s = Settings::load_from(&dir).unwrap();
        assert!(s.vault_enabled);
    }

    #[test]
    fn unknown_field_errors() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.toml"), "completely_made_up = true\n").unwrap();
        assert!(Settings::load_from(&dir).is_err());
    }

    #[test]
    fn malformed_file_errors() {
        let dir = fresh_dir();
        std::fs::write(dir.join("settings.toml"), "not valid = toml =").unwrap();
        assert!(Settings::load_from(&dir).is_err());
    }
}
