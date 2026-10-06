use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use include_dir::{Dir, include_dir};

use crate::config::config_values::{self, ConfigValues};
use crate::config::merge::normalize_env;
use crate::config::{LayeredConfig, ResolvedConfig};
use crate::test_support::block_on;

static FIXTURES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/config/tests/fixtures");

/// A temp dir with separate `home/` and `project/` subdirs, removed on drop
/// (no `tempfile` dep in this crate).
struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "airlock-layers-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("home")).unwrap();
        std::fs::create_dir_all(root.join("project")).unwrap();
        Self { root }
    }

    fn home(&self) -> PathBuf {
        self.root.join("home")
    }

    fn project(&self) -> PathBuf {
        self.root.join("project")
    }

    /// Write `content` to `rel` under the home dir.
    fn home_file(&self, rel: &str, content: &str) {
        write(&self.home().join(rel), content);
    }

    /// Write `content` to `rel` under the project dir.
    fn project_file(&self, rel: &str, content: &str) {
        write(&self.project().join(rel), content);
    }

    fn discover(&self) -> anyhow::Result<LayeredConfig> {
        LayeredConfig::load_from(&self.home(), &self.project())
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

/// The resolved config of `layers`, with the enabled packs.
fn resolved(layers: &LayeredConfig) -> ResolvedConfig {
    block_on(layers.resolve(
        &crate::packs::init().unwrap(),
        &crate::config::ConfigOverrides::default(),
    ))
    .unwrap()
}

/// The resolved config values of `layers`.
fn values(layers: &LayeredConfig) -> ConfigValues {
    resolved(layers).values
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

#[test]
fn no_files_yield_no_layers() {
    let fx = Fixture::new();
    let layers = fx.discover().unwrap();
    assert!(layers.user.is_empty());
    assert!(layers.local.is_none());
    assert!(layers.project.is_empty());
    assert!(!layers.has_project_config());
}

#[test]
fn state_file_alone_is_not_a_layer() {
    let fx = Fixture::new();
    fx.project_file(".airlock/sandbox/installs.json", "{}");
    let layers = fx.discover().unwrap();
    assert!(!layers.has_project_config());
}

#[test]
fn project_and_user_slots() {
    let fx = Fixture::new();
    fx.home_file(".airlock/config.toml", "[vm]\ncpus = 1\n");
    fx.home_file(".airlock.yaml", "vm:\n  cpus: 2\n");
    fx.project_file("airlock.json", r#"{"vm":{"cpus":3}}"#);
    fx.project_file("airlock.local.toml", "[vm]\ncpus = 4\n");
    let layers = fx.discover().unwrap();
    assert_eq!(layers.user.len(), 2);
    assert_eq!(layers.project.len(), 2);
    assert!(layers.has_project_config());
    assert_eq!(values(&layers).vm.cpus, 4);
}

#[test]
fn user_files_alone_are_not_project_level() {
    let fx = Fixture::new();
    fx.home_file(".airlock.toml", "[vm]\ncpus = 1\n");
    let layers = fx.discover().unwrap();
    assert_eq!(layers.user.len(), 1);
    assert!(!layers.has_project_config());
}

#[test]
fn precedence_user_local_project() {
    let fx = Fixture::new();
    fx.home_file(
        ".airlock.toml",
        "[vm]\nimage = \"user:1\"\ncpus = 1\nmemory = \"1 GB\"\n",
    );
    fx.project_file(
        ".airlock/airlock.toml",
        "[vm]\nimage = \"local:1\"\ncpus = 2\n",
    );
    fx.project_file("airlock.toml", "[vm]\nimage = \"project:1\"\n");
    let config = values(&fx.discover().unwrap());
    assert_eq!(config.vm.image.name, "project:1", "project overrides local");
    assert_eq!(config.vm.cpus, 2, "local overrides user");
    assert_eq!(
        config.vm.memory,
        smart_config::ByteSize(1024 * 1024 * 1024),
        "user value kept when no later layer sets it"
    );
}

#[test]
fn presets_sit_below_every_layer() {
    // A preset named in a project file must still lose to a user file.
    let fx = Fixture::new();
    fx.home_file(".airlock.toml", "[env]\nIS_SANDBOX = \"0\"\n");
    fx.project_file("airlock.toml", "presets = [\"claude-code\"]\n");
    let config = values(&fx.discover().unwrap());
    assert_eq!(config.env["IS_SANDBOX"].value, "0");
    assert!(config.network.rules.contains_key("claude-code"));
}

#[test]
fn local_plain_env_string_keeps_preset_mask() {
    let fx = Fixture::new();
    fx.project_file("airlock.toml", "presets = [\"claude-code\"]\n");
    fx.project_file(
        ".airlock/airlock.toml",
        "[env]\nCLAUDE_CODE_OAUTH_TOKEN = \"${OTHER_TOKEN}\"\n",
    );
    let config = values(&fx.discover().unwrap());
    let var = &config.env["CLAUDE_CODE_OAUTH_TOKEN"];
    assert_eq!(var.value, "${OTHER_TOKEN}");
    assert!(var.mask, "a plain string must not un-mask the preset entry");
}

#[test]
fn project_read_error_fails_closed() {
    let fx = Fixture::new();
    std::fs::create_dir_all(fx.project().join("airlock.toml")).unwrap();
    assert!(fx.discover().is_err());
}

/// Every fixture that needs no fixture-only preset resolves through the
/// layer API exactly like a direct preset application + parse.
#[test]
fn fixtures_load_unchanged() {
    let mut checked = 0;
    for file in FIXTURES.files() {
        let content = std::str::from_utf8(file.contents()).unwrap();
        let mut value: serde_json::Value = toml::from_str(content).unwrap();
        if value.get("presets").is_some() {
            continue;
        }
        let fx = Fixture::new();
        fx.project_file("airlock.toml", content);
        let via_layers = values(&fx.discover().unwrap());
        normalize_env(&mut value);
        let direct = config_values::parse(value).unwrap();
        assert_eq!(
            serde_json::to_value(&via_layers).unwrap(),
            serde_json::to_value(&direct).unwrap(),
            "fixture {}",
            file.path().display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no fixture was checked");
}

#[test]
fn local_slot_is_project_level() {
    let fx = Fixture::new();
    fx.project_file(".airlock/airlock.toml", "[vm]\ncpus = 3\n");
    let layers = fx.discover().unwrap();
    let local = layers.local.as_ref().expect("local layer");
    assert!(local.origin.ends_with(".airlock/airlock.toml"));
    assert!(layers.user.is_empty());
    assert!(layers.has_project_config());
    assert_eq!(values(&layers).vm.cpus, 3);
}

#[test]
fn local_slot_probes_extensions() {
    let fx = Fixture::new();
    fx.project_file(".airlock/airlock.yaml", "vm:\n  cpus: 5\n");
    assert_eq!(values(&fx.discover().unwrap()).vm.cpus, 5);
}

#[test]
fn precedence_user_local_project_files() {
    let fx = Fixture::new();
    fx.home_file(
        ".airlock/airlock.toml",
        "[vm]\nimage = \"user:0\"\ncpus = 1\nmemory = \"1 GB\"\n[env]\nA = \"user\"\nB = \"user\"\nC = \"user\"\nD = \"user\"\nE = \"user\"\n",
    );
    fx.home_file(".airlock/config.toml", "[env]\nA = \"config\"\n");
    fx.home_file(".airlock.toml", "[env]\nA = \"home\"\nB = \"home\"\n");
    fx.project_file(
        ".airlock/airlock.toml",
        "[env]\nC = \"local\"\nD = \"local\"\n",
    );
    fx.project_file("airlock.toml", "[env]\nD = \"project\"\n");
    let layers = fx.discover().unwrap();
    assert_eq!(layers.user.len(), 3);
    let config = values(&layers);
    let env = |k: &str| config.env[k].value.clone();
    assert_eq!(env("A"), "home", "~/.airlock.toml overrides config.toml");
    assert_eq!(
        env("B"),
        "home",
        "~/.airlock.toml overrides ~/.airlock/airlock.toml"
    );
    assert_eq!(env("C"), "local", "local overrides user");
    assert_eq!(env("D"), "project", "airlock.toml overrides local");
    assert_eq!(env("E"), "user", "~/.airlock/airlock.toml is read");
}

#[test]
fn home_airlock_file_is_below_config_file() {
    let fx = Fixture::new();
    fx.home_file(".airlock/airlock.toml", "[vm]\ncpus = 1\n");
    fx.home_file(".airlock/config.toml", "[vm]\ncpus = 2\n");
    let layers = fx.discover().unwrap();
    assert_eq!(layers.user.len(), 2);
    assert!(!layers.has_project_config());
    assert_eq!(values(&layers).vm.cpus, 2);
}

/// In the home directory, `.airlock/airlock.toml` is the local project
/// file, not also a user file.
#[test]
fn home_as_project_reads_local_file_once() {
    let fx = Fixture::new();
    fx.project_file(
        ".airlock/airlock.toml",
        "[packs]\npython = { version = 1 }\n",
    );
    let layers = LayeredConfig::load_from(&fx.project(), &fx.project()).unwrap();
    assert!(layers.user.is_empty());
    assert!(layers.local.is_some());
    let resolved = resolved(&layers);
    assert_eq!(resolved.packs.len(), 1);
}

/// A leftover `[tools]` table (unreleased) has no effect in any file.
#[test]
fn leftover_tools_table_is_ignored() {
    let fx = Fixture::new();
    fx.home_file(".airlock/config.toml", "[tools]\npython = {}\n");
    fx.project_file("airlock.toml", "[tools]\nclaude = {}\nnope = 1\n");
    let resolved = resolved(&fx.discover().unwrap());
    assert!(resolved.packs.is_empty());
    assert!(
        !resolved
            .values
            .network
            .rules
            .contains_key("python-packages")
    );
}
