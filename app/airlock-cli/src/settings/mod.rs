//! Application-wide user settings.
//!
//! Loads the user's settings from the airlock directory `~/.airlock`. The settings
//! file can be TOML, JSON or YAML. The CLI loads it once when it starts. If
//! the file does not exist, the defaults apply, so `airlock` works without
//! configuration.

pub(crate) mod keys;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow, bail};
pub use keys::KeyList;
use serde::{Deserialize, Serialize};
use smart_config::{ConfigRepository, ConfigSchema, DescribeConfig, DeserializeConfig, Json};

use crate::config::de::format_error;
use crate::config::files::{EXTENSIONS, parse_file};
use crate::vault::VaultStorageType;

/// All user settings. Add new fields here. The default of each field must
/// keep `airlock` usable without a settings file.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct Settings {
    /// Vault configuration. It is in the `[vault]` table, so future vault
    /// settings (passphrase cache policy, custom storage path, ...) can go
    /// next to `storage`, not in the top-level namespace.
    #[config(nest)]
    pub vault: VaultSettings,
    /// Monitor TUI settings (buffer limits, terminal scrollback, key
    /// bindings). These are personal preferences, so they are not in the
    /// per-project `airlock.toml`.
    #[config(nest)]
    pub monitor: MonitorSettings,
    /// Default answers of the setup wizard of `airlock start`.
    #[config(nest)]
    pub wizard_defaults: WizardDefaults,
    /// Security settings that make the sandbox less isolated if they
    /// change. The defaults are strict.
    #[config(nest)]
    pub security: SecuritySettings,
    /// Where `airlock start` puts the data of a new sandbox:
    ///  * `cache-dir` (default): in the airlock data directory, out of the
    ///    reach of the sandbox guest
    ///  * `project-dir`: in `.airlock/sandbox` in the project. Existing
    ///    project sandboxes then stay there without a question.
    #[config(default)]
    pub sandbox_location: SandboxLocation,
    /// Airlock data directory: the database, the sandboxes and the image
    /// cache. `~` expands to the home directory. The default is
    /// `airlock` in the user data directory of the platform.
    pub data_dir: Option<String>,
}

/// Settings under the `[security]` table.
#[derive(Clone, Debug, Default, DescribeConfig, DeserializeConfig)]
pub struct SecuritySettings {
    /// Allow directory mounts (also the project) that contain the home
    /// directory or the airlock data directory. Such a mount gives the
    /// sandbox all secrets in it, for example SSH keys and the vault. The
    /// default (false) refuses to start the sandbox.
    #[config(default)]
    pub insecure_mounts: bool,
}

/// Settings under the `[wizard_defaults]` table.
#[derive(Clone, Debug, Default, DescribeConfig, DeserializeConfig)]
pub struct WizardDefaults {
    /// The option of the start bar at the start of the setup wizard:
    ///  * `start-and-share` (default): start with a shareable config
    ///    (`airlock.toml`)
    ///  * `start`: start with a local config (in the sandbox directory, out
    ///    of the repository)
    #[config(default)]
    pub start: WizardStart,
}

/// The start option that the setup wizard selects first. Matches
/// `settings.wizard_defaults.start`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum WizardStart {
    /// Start with a local config.
    Start,
    /// Start with a shareable config.
    #[default]
    StartAndShare,
}

impl smart_config::de::WellKnown for WizardStart {
    type Deserializer =
        smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// Location of the data of a new sandbox. Matches `sandbox_location`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SandboxLocation {
    /// `.airlock/sandbox` in the project directory.
    ProjectDir,
    /// `boxes/<id>` in the airlock data directory.
    #[default]
    CacheDir,
}

impl smart_config::de::WellKnown for SandboxLocation {
    type Deserializer =
        smart_config::de::Serde<{ smart_config::metadata::BasicTypes::STRING.raw() }>;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// Settings under the `[vault]` table.
#[derive(Clone, Debug, Default, DescribeConfig, DeserializeConfig)]
pub struct VaultSettings {
    /// Backend that stores user secrets and registry credentials:
    ///  * `keyring` (default): the OS keychain (macOS Keychain or Linux
    ///    Secret Service)
    ///  * `encrypted-file`: a JSON file encrypted with a passphrase
    ///  * `file`: a plaintext file with mode 0600
    ///  * `disabled`: no vault.
    #[config(default)]
    pub storage: VaultStorageType,
}

/// Settings under the `[monitor]` table.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct MonitorSettings {
    /// Buffer limits and scrollback for the TUI.
    #[config(nest)]
    pub buffers: MonitorBuffers,
    /// Key bindings for each action. Action names are the kebab-case names
    /// in `airlock_monitor::keys::SPEC`. Each value is one key string
    /// (`back = "q"`) or an array (`cancel = ["esc", "x"]`). Actions that
    /// are not set keep their defaults.
    #[config(default)]
    pub keys: BTreeMap<String, KeyList>,
}

impl MonitorSettings {
    /// Build the TUI settings: buffer limits, scrollback, and the key
    /// bindings over the defaults.
    /// Returns:
    ///   The TUI settings, or error if a key binding is not valid. The error
    ///   lists all problems, one on each line.
    pub fn tui_settings(&self) -> Result<airlock_monitor::TuiSettings, String> {
        Ok(airlock_monitor::TuiSettings {
            max_http_requests: self.buffers.http,
            max_tcp_connections: self.buffers.tcp,
            scrollback: self.buffers.scrollback,
            keys: keys::into_bindings(&self.keys)?,
        })
    }
}

/// Settings under the `[monitor.buffers]` table. The defaults are the values
/// that were hard-coded in `airlock-monitor` before.
#[derive(Clone, Debug, DescribeConfig, DeserializeConfig)]
pub struct MonitorBuffers {
    /// Maximum number of HTTP request entries in the monitor buffer. At the
    /// limit, the oldest entries are dropped.
    #[config(default_t = 100)]
    pub http: usize,
    /// Maximum number of TCP connection entries in the monitor buffer. At
    /// the limit, the oldest entries are dropped.
    #[config(default_t = 100)]
    pub tcp: usize,
    /// Number of scrollback rows in the embedded vt100 terminal of the
    /// sandbox tab. More rows use more memory and let the user scroll back
    /// farther in the sandbox session.
    #[config(default_t = 1000)]
    pub scrollback: u16,
}

impl Settings {
    /// Get the airlock directory of the user (`~/.airlock`).
    pub fn dir() -> Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory missing"))?;
        Ok(home.join(".airlock"))
    }

    /// Get the airlock data directory: `data_dir` with `~` expanded, or
    /// `airlock` in the user data directory of the platform.
    pub fn data_dir(&self) -> Result<PathBuf> {
        let home = dirs::home_dir().ok_or_else(|| anyhow!("home directory missing"))?;
        if let Some(dir) = &self.data_dir {
            let dir = crate::util::expand_tilde(dir, &home);
            if !dir.is_absolute() {
                bail!("data_dir must be an absolute path: {}", dir.display());
            }
            return Ok(dir);
        }
        crate::cache::default_data_dir()
    }

    /// Get the display path of the TOML settings file. For error messages
    /// that tell the user to create or edit the file.
    pub fn expected_path() -> PathBuf {
        PathBuf::from("~/.airlock/settings.toml")
    }

    /// Load the settings from the first `settings.*` file in the airlock
    /// directory `dir` (`~/.airlock`, see [`Self::dir`]).
    /// Returns:
    ///   The settings, or the defaults if no file exists. Error if the file
    ///   is malformed, so the user does not get defaults without notice.
    pub fn load_from(dir: &Path) -> Result<Self> {
        // Same extension order (TOML, JSON, YAML) as the project config
        // loader. If there are multiple files, TOML wins. Thus a stray
        // `settings.json` cannot hide the user's primary `settings.toml`.
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
        // No file: still parse an empty source with smart-config, so the
        // per-field `default_t` annotations apply. This is important for the
        // `[monitor.buffers]` defaults, which `derive(Default)` would make 0.
        parse_settings(serde_json::Value::Object(serde_json::Map::new()))
    }
}

/// Parse the settings from the parsed file (as a JSON object) with
/// smart-config. Uses the same pipeline as the project config loader.
/// Returns:
///   The settings, or a structured parse error for type mismatches.
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
    //! Tests for the load of the user settings file.

    use super::*;
    use crate::test_cfg::temp_dir;

    /// Test that the settings come from the first file in the order TOML,
    /// JSON, YAML, and that the defaults apply when there is no file.
    ///   1. Load from a missing directory and check the defaults
    ///   2. Add a YAML file and check that it loads
    ///   3. Add a JSON file and check that it wins over the YAML file
    ///   4. Add a TOML file and check that it wins, and that the fields it
    ///      does not set keep their defaults
    #[test]
    fn settings_load_from_first_file_by_format_or_defaults() {
        let dir = temp_dir();
        let s = Settings::load_from(&dir.path().join("missing")).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::Keyring);
        assert_eq!(s.monitor.buffers.http, 100);
        assert_eq!(s.monitor.buffers.scrollback, 1000);
        assert_eq!(s.wizard_defaults.start, WizardStart::StartAndShare);
        assert_eq!(s.sandbox_location, SandboxLocation::CacheDir);
        assert_eq!(s.data_dir, None);

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
            "vault.storage = \"file\"\nwizard_defaults.start = \"start\"\n\
             sandbox_location = \"project-dir\"\ndata_dir = \"~/airlock-data\"\n\
             [monitor.buffers]\nhttp = 5\n",
        )
        .unwrap();
        let s = Settings::load_from(dir.path()).unwrap();
        assert_eq!(s.vault.storage, VaultStorageType::File);
        assert_eq!(s.monitor.buffers.http, 5);
        assert_eq!(s.monitor.buffers.tcp, 100);
        assert_eq!(s.wizard_defaults.start, WizardStart::Start);
        assert_eq!(s.sandbox_location, SandboxLocation::ProjectDir);
        assert_eq!(s.data_dir.as_deref(), Some("~/airlock-data"));
    }

    /// Test that a settings file with bad syntax or a bad value fails the
    /// load, so that the user does not get the defaults without notice.
    ///   1. Write a file with bad TOML, then a file with an unknown value
    ///   2. Check that each load fails with the file name in the error
    #[test]
    fn malformed_or_invalid_settings_file_fails_load() {
        for content in [
            "not valid = toml =",
            "vault.storage = \"typo\"\n",
            "sandbox_location = \"typo\"\n",
        ] {
            let dir = temp_dir();
            std::fs::write(dir.path().join("settings.toml"), content).unwrap();
            let err = format!("{:#}", Settings::load_from(dir.path()).unwrap_err());
            assert!(err.contains("settings.toml"), "{err}");
        }
    }
}
