//! Tests for config layers: which files the load reads, their precedence,
//! how packs and presets fit between them, and load errors.

use smart_config::ByteSize;

use crate::config::ConfigOverrides;
use crate::config::config_values::Policy;
use crate::test_cfg::{ConfigDirs, resolve_layers};

/// Test that user, local and project files merge in precedence order. Lists
/// concatenate, a null does not remove a lower value, and a CLI override wins.
///   1. Write user files and project files that set the same keys
///   2. Load the layers with a network policy override
///   3. Check which file wins for each env key and VM setting
///   4. Check the services, the concatenated allow list and the policy
#[test]
fn user_local_and_project_files_merge_in_precedence_order() {
    let dirs = ConfigDirs::new();
    dirs.user_file(
        ".airlock/airlock.toml",
        r#"
        [vm]
        image = "user:0"
        cpus = 1
        memory = "1 GB"

        [env]
        A = "user"
        B = "user"
        C = "user"
        D = "user"
        E = "user"

        [network.services]
        anthropic = true
        openai = true

        [network.rules.api]
        allow = ["user.example.com"]
        "#,
    )
    .user_file(".airlock/config.json", r#"{"env": {"A": "config"}}"#)
    // The null `memory` must not remove the 1 GB from the lower file.
    .user_file(
        ".airlock.yaml",
        "vm:\n  memory:\nenv:\n  A: home\n  B: home\n",
    )
    .project_file(
        ".airlock/airlock.yml",
        "vm:\n  cpus: 2\nenv:\n  C: local\n  D: local\n\
         network:\n  rules:\n    api:\n      allow: [local.example.com]\n",
    )
    .project_file(
        "airlock.toml",
        r#"
        [vm]
        image = "project:1"

        [env]
        D = "project"

        [network.services]
        openai = false

        [network.rules.api]
        allow = ["project.example.com"]
        "#,
    )
    // `airlock.toml` hides `airlock.json`, so the load does not read it.
    .project_file("airlock.json", r#"{"env": {"D": "shadowed"}}"#)
    .project_file("airlock.local.toml", "[env]\nF = \"project-local\"\n");

    let layers = dirs.load().unwrap();
    assert!(layers.has_project_config());
    let resolved = resolve_layers(
        &layers,
        ConfigOverrides {
            network_policy: Some(Policy::DenyByDefault),
        },
    )
    .unwrap();
    let config = resolved.values;
    let env = |key: &str| config.env[key].value.as_str();
    assert_eq!(env("A"), "home");
    assert_eq!(env("B"), "home");
    assert_eq!(env("C"), "local");
    assert_eq!(env("D"), "project");
    assert_eq!(env("E"), "user");
    assert_eq!(env("F"), "project-local");
    assert_eq!(config.vm.image.name, "project:1");
    assert_eq!(config.vm.cpus, 2);
    assert_eq!(config.vm.memory, ByteSize(1024 * 1024 * 1024));
    assert_eq!(
        crate::services::enabled(&config.network.services),
        [crate::services::ServiceId::Anthropic]
    );
    assert_eq!(
        config.network.rules["api"].allow,
        [
            "user.example.com",
            "local.example.com",
            "project.example.com"
        ]
    );
    assert_eq!(config.network.policy, Policy::DenyByDefault);
}

/// Test that a pack in a project file overrides user files, that a file's
/// own value overrides its own pack, and that the project file
/// `airlock.toml` overrides a pack in the local file.
///   1. Set a user image and an alpine pack in the local file, and check that
///      the pack image wins
///   2. Also set an image in the local file and check that it wins over the
///      pack of the same file
///   3. Set an image in `airlock.toml` and check that it wins
#[test]
fn project_pack_overrides_user_files_and_its_own_file_overrides_pack() {
    let dirs = ConfigDirs::new();
    dirs.user_file(".airlock.toml", "[vm]\nimage = \"user:1\"\n")
        .project_file(
            ".airlock/airlock.toml",
            "[packs]\nalpine = { version = 1 }\n",
        );
    assert_eq!(dirs.values().vm.image.name, "alpine:latest");

    dirs.project_file(
        ".airlock/airlock.toml",
        "[packs]\nalpine = { version = 1 }\n[vm]\nimage = \"local:1\"\n",
    );
    assert_eq!(dirs.values().vm.image.name, "local:1");

    dirs.project_file("airlock.toml", "[vm]\nimage = \"project:1\"\n");
    assert_eq!(dirs.values().vm.image.name, "project:1");
}

/// Test that a preset applies below all config files, also the user file. A
/// value from a file keeps the mask flag of the preset entry.
///   1. Set a preset in `airlock.toml` and override its keys in the user file
///      and the local file
///   2. Check that the user value wins and the preset rule exists
///   3. Check that the local token value wins and is still masked
#[test]
fn presets_list_applies_below_every_file() {
    let dirs = ConfigDirs::new();
    dirs.user_file(".airlock.toml", "[env]\nIS_SANDBOX = \"0\"\n")
        .project_file("airlock.toml", "presets = [\"claude-code\"]\n")
        .project_file(
            ".airlock/airlock.toml",
            "[env]\nCLAUDE_CODE_OAUTH_TOKEN = \"${OTHER_TOKEN}\"\n",
        );
    let config = dirs.values();
    assert_eq!(config.env["IS_SANDBOX"].value, "0");
    assert!(config.network.rules.contains_key("claude-code"));
    let token = &config.env["CLAUDE_CODE_OAUTH_TOKEN"];
    assert_eq!(token.value, "${OTHER_TOKEN}");
    assert!(token.mask);
}

/// Test that only config files make a project config. Sandbox files in
/// `.airlock` and user files do not.
///   1. Check that an empty project has no project config
///   2. Add a sandbox file and a user config file
///   3. Check that there is still no project config, and the user value
///      applies
#[test]
fn project_without_config_files_has_no_project_config() {
    let dirs = ConfigDirs::new();
    assert!(!dirs.load().unwrap().has_project_config());

    dirs.project_file(".airlock/sandbox/installs.json", "{}")
        .user_file(".airlock/config.toml", "[vm]\ncpus = 3\n");
    let layers = dirs.load().unwrap();
    assert!(!layers.has_project_config());
    assert_eq!(dirs.values().vm.cpus, 3);
}

/// Test that the local file `.airlock/airlock.yaml` alone is a project config.
///   1. Write only the local file
///   2. Check that the project has a project config and its value applies
#[test]
fn local_project_file_alone_is_project_config() {
    let dirs = ConfigDirs::new();
    dirs.project_file(".airlock/airlock.yaml", "vm:\n  cpus: 5\n");
    assert!(dirs.load().unwrap().has_project_config());
    assert_eq!(dirs.values().vm.cpus, 5);
}

/// Test that in the home directory, the load reads `.airlock/airlock.toml`
/// only as a project file. As a user file, its `[packs]` table is an error.
///   1. Write a local file with a pack, and use the project as the home
///   2. Load the layers and check that there is a project config
///   3. Resolve and check that the pack applies
#[test]
fn project_in_home_directory_reads_local_file_as_project_file_only() {
    let dirs = ConfigDirs::new();
    dirs.project_file(
        ".airlock/airlock.toml",
        "[packs]\npython = { version = 1 }\n",
    );
    let layers = crate::config::LayeredConfig::load_from(&dirs.project(), &dirs.project()).unwrap();
    assert!(layers.has_project_config());
    let resolved = resolve_layers(&layers, ConfigOverrides::default()).unwrap();
    assert_eq!(resolved.packs.len(), 1);
}

/// Test that a `[packs]` table in a user file is an error that names the
/// file. Only project config files can install packs.
///   1. Write a user file with a pack
///   2. Check the full error text
#[test]
fn packs_table_in_user_file_is_error() {
    let dirs = ConfigDirs::new();
    dirs.user_file(
        ".airlock/config.toml",
        "[packs]\npython = { version = 1 }\n",
    );
    let err = format!("{:#}", dirs.load().err().unwrap());
    assert_eq!(
        err,
        format!(
            "`[packs]` is allowed only in project config files (airlock.toml, \
             .airlock/airlock.toml); remove it from {}",
            dirs.home().join(".airlock/config.toml").display()
        )
    );
}

/// Test that an unreadable or malformed config file stops the load.
///   1. Make a directory in place of `airlock.toml` and check the read error
///   2. Make a directory in place of the user file and check the error
///   3. Write malformed YAML and check that the error names the file
#[test]
fn unreadable_or_malformed_config_file_fails_load() {
    let dirs = ConfigDirs::new();
    std::fs::create_dir_all(dirs.project().join("airlock.toml")).unwrap();
    let err = format!("{:#}", dirs.load().err().unwrap());
    assert!(err.starts_with("read config file "), "{err}");

    let dirs = ConfigDirs::new();
    std::fs::create_dir_all(dirs.home().join(".airlock.toml")).unwrap();
    assert!(dirs.load().is_err());

    let dirs = ConfigDirs::new();
    dirs.project_file("airlock.local.yml", "vm: [unclosed\n");
    let err = format!("{:#}", dirs.load().err().unwrap());
    assert!(
        err.starts_with(&dirs.project_origin("airlock.local.yml")),
        "{err}"
    );
}
