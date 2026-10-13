//! Tests for the config export: the project document and the resolved document,
//! and the parse of the exported text as a config file.

use std::path::Path;

use serde_json::json;

use crate::config::export::{self, Format};
use crate::config::{ConfigOverrides, LayeredConfig, ResolvedConfig, files};
use crate::test_cfg::{ConfigDirs, block_on, temp_dir};

/// Test that the project document has only the merged project-level files,
/// in the form that the user wrote.
///   1. Write a user file, the local file and two project files
///   2. Make the project document
///   3. Check that the project files win over the local file, and that no
///      user value is in the document
///   4. Check that the `presets` list, the plain env strings, the masked
///      env tables and the `[packs]` table stay
#[test]
fn project_document_has_merged_project_files_without_user_files() {
    let dirs = ConfigDirs::new();
    dirs.user_file(
        ".airlock.toml",
        "[vm]\ncpus = 8\n[env]\nUSER_ONLY = \"user\"\nSHARED = \"user\"\n",
    )
    .project_file(
        ".airlock/airlock.toml",
        r#"
        presets = ["rust"]

        [vm]
        cpus = 2
        memory = "2 GB"

        [env]
        SHARED = "local"
        LOCAL = "local"
        "#,
    )
    .project_file(
        "airlock.toml",
        r#"
        [packs]
        python = { version = 1 }

        [vm]
        cpus = 4

        [env]
        SHARED = "project"
        TOKEN = { value = "${HOST_TOKEN}", mask = true }

        [network.rules.api]
        allow = ["api.example.com"]
        "#,
    )
    .project_file("airlock.local.toml", "[env]\nPROJECT_LOCAL = \"x\"\n");

    let document = dirs.load().unwrap().project_document().unwrap();

    assert_eq!(
        document,
        json!({
            "presets": ["rust"],
            "packs": {"python": {"version": 1}},
            "vm": {"cpus": 4, "memory": "2 GB"},
            "env": {
                "SHARED": "project",
                "LOCAL": "local",
                "TOKEN": {"value": "${HOST_TOKEN}", "mask": true},
                "PROJECT_LOCAL": "x",
            },
            "network": {"rules": {"api": {"allow": ["api.example.com"]}}},
        })
    );
}

/// Test that the resolved document of a config, written in each format and
/// read back as one project file, resolves to the same config. Thus the
/// user can use the `airlock config --resolved` output as a config file.
///   1. Resolve a config that sets each section and enables two packs
///   2. Make the resolved document, check that plain env entries are strings,
///      and write it as TOML, JSON and YAML
///   3. Parse each text as one project file and resolve it
///   4. Check that the resolved document of the result is equal to the first
///   5. Save the config files as the config of the last run, read them back
///      and check that they resolve to the same resolved document
#[test]
fn resolved_document_in_each_format_resolves_to_same_config() {
    let data = temp_dir();
    let resolve = |layers: &LayeredConfig| -> ResolvedConfig {
        let packs = crate::packs::init_with_sample();
        block_on(layers.resolve(data.path(), &packs, &ConfigOverrides::default())).unwrap()
    };
    let user = toml::from_str("[vm]\nmemory = \"3 GB\"\n[env]\nFROM_USER = \"u\"\n").unwrap();
    let project = toml::from_str(FULL_CONFIG).unwrap();
    let layers = LayeredConfig::from_values(
        vec![("user.toml", user)],
        None,
        vec![("airlock.toml", project)],
    )
    .unwrap();
    let expected = export::resolved_document(&resolve(&layers)).unwrap();
    // The packs merge in. Thus the document has their values, but no
    // `[packs]` table.
    assert!(expected.get("packs").is_none());
    assert!(expected["network"]["rules"]["rust-lang"].is_object());
    assert_eq!(expected["env"]["PLAIN"], "static");
    assert_eq!(expected["env"]["TOKEN"]["mask"], true);

    for (format, ext) in [
        (Format::Toml, "toml"),
        (Format::Json, "json"),
        (Format::Yaml, "yaml"),
    ] {
        let text = export::render(&expected, format).unwrap();
        let path = Path::new("airlock").with_extension(ext);
        let value = files::parse_file(&path, &text).unwrap();
        let layers = LayeredConfig::from_values(vec![], None, vec![(ext, value)]).unwrap();
        let actual = export::resolved_document(&resolve(&layers)).unwrap();
        assert_eq!(actual, expected, "{ext}:\n{text}");
    }

    let sandbox = temp_dir();
    let sandbox_dir = crate::util::VerifiedPath::new_follow(sandbox.path(), None).unwrap();
    crate::start::config_change::save(&sandbox_dir, &layers, &crate::project::HostPaths::new())
        .unwrap();
    let stored = crate::project::read_run_meta(&sandbox_dir)
        .last_run_config
        .unwrap();
    let stored: LayeredConfig = serde_json::from_value(stored).unwrap();
    assert_eq!(
        export::resolved_document(&resolve(&stored)).unwrap(),
        expected
    );
}

/// Project file with a value in each config section.
const FULL_CONFIG: &str = r#"
[packs]
rust = { version = 1, args = { toolchain = "nightly", allow-cargo = false } }
claude = { version = 1 }

[vm]
cpus = 3
kvm = true
harden = false

[vm.image]
name = "debian:12"
resolution = "podman"
insecure = true
pull-policy = "if-changed"

[network]
policy = "deny-by-default"

[network.rules.api]
allow = ["api.example.com:443", "*.example.org"]
deny = ["internal.example.com:*"]
inject = ["TOKEN"]

[network.rules.off]
enabled = false
allow = ["off.example.com"]
passthrough = true

[network.middleware.mw]
target = ["api.example.com:*"]
env = { KEY = "${HOST_KEY}" }
script = "return"

[network.ports.app]
host = [8080, "9000:8081"]
guest = ["5432:5433"]

[network.sockets.docker]
host = "~/.docker/run/docker.sock:/var/run/docker.sock"

[network.services]
anthropic = true

[mounts.data]
source = "./data"
target = "/data"
read_only = true
missing = "create-file"
create_mode = "600"
file_content = "hello"

[mounts.off]
enabled = false
source = "/src"
target = "/dst"

[disk]
size = "20 GB"

[disk.cache.cargo]
paths = ["/root/.cargo/registry"]

[env]
PLAIN = "static"
TOKEN = { value = "${HOST_TOKEN}", mask = true }
MAYBE = { value = "${HOST_MAYBE}", optional = true }

[daemons.docker]
command = ["dockerd"]
cwd = "/var/lib/docker"
signal = "SIGINT"
timeout = 30
restart = "on-failure"
max_restarts = 0
harden = false
env = { DOCKER_HOST = "unix:///var/run/docker.sock" }

[mask.secrets]
paths = ["secrets", "infra/keys"]

[clipboard]
copy = true
copy_limit = "2 MB"
paste = true
"#;
