//! Application-wide user settings loaded from `~/.airlock/settings.*`.
//!
//! Resolved once at `main` into the [`crate::context::Context`]. Shares the
//! smart-config pipeline with the project-level `airlock.toml` loader
//! (`crate::config::files`): same TOML/JSON/YAML auto-detect,
//! same parse-error formatting. Missing file → defaults, which keeps
//! `airlock` usable with zero configuration.

pub(crate) mod keys;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
pub use keys::KeyList;
use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, DeserializeConfig, Json};

use crate::config::de::format_error;
use crate::config::files::{EXTENSIONS, parse_file};
use crate::vault::VaultStorageType;

/// All user-tunable settings. Add fields here; the default for each
/// field must keep `airlock` usable without a settings file.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct Settings {
    /// Vault configuration. Nested under `[vault]` so future vault-related
    /// knobs (passphrase caching policy, custom storage path, ...) fit
    /// alongside `storage` without polluting the top-level namespace.
    #[config(nest)]
    pub vault: VaultSettings,
    /// Monitor TUI tuning (buffer caps, terminal scrollback, key
    /// bindings). Personal preferences kept out of the per-project
    /// `airlock.toml`.
    #[config(nest)]
    pub monitor: MonitorSettings,
}

/// Settings under the `[vault]` table.
#[derive(Clone, Debug, Default, DescribeConfig, DeserializeConfig)]
pub struct VaultSettings {
    /// Which backend stores user secrets and registry credentials.
    /// Defaults to `keyring` — the OS keychain (macOS Keychain /
    /// Linux Secret Service). Switch to `encrypted-file` for a
    /// passphrase-encrypted JSON file, `file` for mode-0600 plaintext,
    /// or `disabled` to turn the vault off entirely.
    #[config(default)]
    pub storage: VaultStorageType,
}

/// Settings under the `[monitor]` table.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct MonitorSettings {
    /// Buffer caps and scrollback for the TUI.
    #[config(nest)]
    pub buffers: MonitorBuffers,
    /// Per-action key bindings. Action names match the canonical
    /// kebab-case list (see `airlock_monitor::keys::SPEC`); each value
    /// is either a single key string (`back = "q"`) or an array
    /// (`cancel = ["esc", "x"]`). Unset actions keep their defaults.
    #[config(default)]
    pub keys: BTreeMap<String, KeyList>,
}

impl MonitorSettings {
    /// Build the TUI settings: buffer caps, scrollback, and the key
    /// bindings resolved over the defaults. Invalid key bindings are an
    /// error listing every problem, one per line.
    pub fn tui_settings(&self) -> Result<airlock_monitor::TuiSettings, String> {
        Ok(airlock_monitor::TuiSettings {
            max_http_requests: self.buffers.http,
            max_tcp_connections: self.buffers.tcp,
            scrollback: self.buffers.scrollback,
            keys: keys::into_bindings(&self.keys)?,
        })
    }
}

/// Settings under the `[monitor.buffers]` table. Defaults match the
/// values previously hard-coded in `airlock-monitor`.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct MonitorBuffers {
    /// Maximum HTTP request entries kept in the monitor buffer.
    /// Once the cap is hit, the oldest entries are dropped.
    #[config(default_t = 100)]
    pub http: usize,
    /// Maximum TCP connection entries kept in the monitor buffer.
    /// Once the cap is hit, the oldest entries are dropped.
    #[config(default_t = 100)]
    pub tcp: usize,
    /// Scrollback rows retained by the embedded vt100 terminal that
    /// drives the sandbox tab. Trades memory for how far back the
    /// user can scroll into the sandbox session.
    #[config(default_t = 1000)]
    pub scrollback: u16,
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

    /// Load settings from the first matching `settings.*` file in the
    /// airlock directory `dir` (`~/.airlock`, see [`Self::dir`]). Missing
    /// file → defaults. Parse errors bubble up so the user notices a
    /// malformed file instead of silently getting defaults.
    pub fn load_from(dir: &Path) -> Result<Self> {
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
        // No file → still go through smart-config with an empty
        // source so per-field `default_t` annotations apply (notably
        // the `keys` defaults, which would be empty under derive(Default)).
        parse_settings(serde_json::Value::Object(serde_json::Map::new()))
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
    use super::*;
    use crate::test_cfg::temp_dir;

    #[test]
    fn settings_load_from_first_file_by_format_or_defaults() {
        let dir = temp_dir();
        let s = Settings::load_from(&dir.path().join("missing")).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::Keyring);
        assert_eq!(s.monitor.buffers.http, 100);
        assert_eq!(s.monitor.buffers.scrollback, 1000);

        std::fs::write(
            dir.path().join("settings.yml"),
            "vault:\n  storage: disabled\n",
        )
        .unwrap();
        let s = Settings::load_from(dir.path()).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::Disabled);

        std::fs::write(
            dir.path().join("settings.json"),
            r#"{"vault": {"storage": "keyring"}}"#,
        )
        .unwrap();
        let s = Settings::load_from(dir.path()).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::Keyring);

        std::fs::write(
            dir.path().join("settings.toml"),
            "vault.storage = \"file\"\n[monitor.buffers]\nhttp = 5\n",
        )
        .unwrap();
        let s = Settings::load_from(dir.path()).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::File);
        assert_eq!(s.monitor.buffers.http, 5);
        assert_eq!(s.monitor.buffers.tcp, 100);
    }

    #[test]
    fn malformed_or_invalid_settings_file_fails_load() {
        for content in ["not valid = toml =", "vault.storage = \"typo\"\n"] {
            let dir = temp_dir();
            std::fs::write(dir.path().join("settings.toml"), content).unwrap();
            let err = format!("{:#}", Settings::load_from(dir.path()).unwrap_err());
            assert!(err.contains("settings.toml"), "{err}");
        }
    }
}
