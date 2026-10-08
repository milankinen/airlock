//! Tests for `[packs]` entries: their order and merge across files, and the
//! config errors for bad entries.

use crate::config::ResolvedConfig;
use crate::packs::ArgValue;
use crate::test_cfg::{ConfigDirs, project_toml_error, resolve_project_toml};

/// Return the name and version of each resolved pack, in order.
fn versions(resolved: &ResolvedConfig) -> Vec<(&str, &str)> {
    resolved
        .packs
        .iter()
        .map(|p| (p.metadata().name.as_str(), p.metadata().version.as_str()))
        .collect()
}

/// Return the value of the arg `key` of the pack `name`, if it is set.
fn arg<'a>(resolved: &'a ResolvedConfig, name: &str, key: &str) -> Option<&'a ArgValue> {
    resolved
        .packs
        .iter()
        .find(|p| p.metadata().name == name)?
        .args()
        .get(key)
}

/// Test that pack entries resolve in catalog order, not file order, and that
/// the file that holds them overrides their values.
///   1. Resolve five packs and an env value in one file
///   2. Check that the packs are in catalog order
///   3. Check the pack rules and env values, and that the file value wins
#[test]
fn pack_entries_apply_in_pack_order_below_their_file() {
    let resolved = resolve_project_toml(
        r#"
        [packs]
        python = { version = 1 }
        claude = { version = 1 }
        docker = { version = 1, enabled = true }
        alpine = { version = 1 }
        rust = { version = "1" }

        [env]
        SSL_CERT_FILE = "/x.pem"
        "#,
    )
    .unwrap();
    assert_eq!(
        versions(&resolved),
        [
            ("alpine", "1"),
            ("claude", "1"),
            ("docker", "1"),
            ("python", "1"),
            ("rust", "1")
        ]
    );
    let config = resolved.values;
    assert!(config.network.rules.contains_key("python-packages"));
    assert!(config.network.rules.contains_key("rust-packages"));
    for key in ["REQUESTS_CA_BUNDLE", "PIP_CERT"] {
        assert_eq!(
            config.env[key].value, "/etc/ssl/certs/ca-certificates.crt",
            "{key}"
        );
        assert!(!config.env[key].mask, "{key}");
    }
    assert_eq!(config.env["SSL_CERT_FILE"].value, "/x.pem");
}

/// Test that the version, args and `enabled` of a pack entry merge across
/// the project files.
///   1. Set pack entries in the local, project and project-local files
///   2. Check that only the enabled packs with a version resolve
///   3. Check the merged and default args of the sample pack
///   4. Check that the python preset applies, and the disabled codex pack
///      does not
#[test]
fn pack_entries_merge_version_args_and_enabled_across_files() {
    let dirs = ConfigDirs::new();
    dirs.project_file(
        ".airlock/airlock.toml",
        r#"
        [packs]
        sample = { version = 1, args = { mode = "slow" } }
        nodejs = { version = 1 }
        python = { enabled = false }
        rust = { enabled = false }
        codex = { version = 1 }
        "#,
    )
    .project_file(
        "airlock.toml",
        r#"
        presets = ["python"]

        [packs]
        sample = { version = 1 }
        nodejs = {}
        "#,
    )
    .project_file(
        "airlock.local.toml",
        // The codex arg is bad, but codex is disabled, so it is not checked.
        "[packs]\nrust = { version = 1, enabled = true }\n\
         codex = { enabled = false, args = { acp = \"broken\" } }\n",
    );
    let resolved = dirs.resolve().unwrap();
    assert_eq!(
        versions(&resolved),
        [("nodejs", "1"), ("rust", "1"), ("sample", "1")]
    );
    assert_eq!(
        arg(&resolved, "sample", "mode"),
        Some(&ArgValue::Text("slow".into()))
    );
    assert_eq!(
        arg(&resolved, "sample", "network"),
        Some(&ArgValue::Bool(true))
    );
    assert_eq!(resolved.values.env["SAMPLE_MODE"].value, "slow");
    assert!(
        resolved
            .values
            .network
            .rules
            .contains_key("python-packages")
    );
    assert!(!resolved.values.env.contains_key("OPENAI_API_KEY"));
}

/// Test that the error for a pack without a version names each file that
/// sets the entry. The user can then find where to add the version.
///   1. Check the error for one file
///   2. Check that the error names both files when two files set the entry
///   3. Check that a presets list does not count as a file that sets it
#[test]
fn missing_version_names_every_contributing_file() {
    assert!(
        project_toml_error("[packs]\nnodejs = {}\n")
            .contains("* `packs.nodejs` needs `version` (set in: airlock.toml): 1")
    );

    let dirs = ConfigDirs::new();
    dirs.project_file(".airlock/airlock.toml", "[packs]\nnodejs = {}\n")
        .project_file("airlock.local.toml", "[packs]\nnodejs = {}\n");
    let err = dirs.error();
    assert!(
        err.contains(&format!(
            "(set in: {}, {})",
            dirs.project_origin(".airlock/airlock.toml"),
            dirs.project_origin("airlock.local.toml")
        )),
        "{err}"
    );

    let dirs = ConfigDirs::new();
    dirs.project_file(".airlock/airlock.toml", "[packs]\ncodex = {}\n")
        .project_file("airlock.toml", "presets = [\"openai-codex\"]\n");
    let err = dirs.error();
    assert!(
        err.contains(&format!(
            "* `packs.codex` needs `version` (set in: {}): 1",
            dirs.project_origin(".airlock/airlock.toml")
        )),
        "{err}"
    );
}

/// Test that all bad pack entries show in one config error, each with a
/// hint. The user can then correct all of them at one time.
///   1. Resolve a `[packs]` table with many different bad entries
///   2. Check that the error has a line for each bad entry
///   3. Check the error for a `packs` value that is not a table
#[test]
fn invalid_pack_entries_are_reported_together() {
    let err = project_toml_error(
        r#"
        [packs]
        nope = { version = 1 }
        claude-code = { version = 1 }
        python = true
        rust = {}
        sample = { version = 1, args = { mode = 1, network = "yes", model = "x" } }
        nodejs = { version = 1, node-version = "lts", enabled = "no" }
        docker = { version = 1, args = { node-version = "lts" } }
        mise = { version = "legacy" }
        claude = { version = "legacy" }
        codex = { version = 2 }
        git = { version = 0 }
        copilot = { version = 1.5 }
        alpine = { version = true }
        debian = { version = -1, args = 1 }
        "#,
    );
    assert!(err.starts_with("invalid configuration\n"), "{err}");
    for line in [
        "* `packs.nope` unknown pack (known: alpine, debian, claude, codex, copilot, docker, \
         git, mise, nodejs, python, rust, sample) (set in: airlock.toml)",
        "* `packs.claude-code` unknown pack (known: alpine, debian, claude, codex, copilot, \
         docker, git, mise, nodejs, python, rust, sample); `claude-code` is a list-form name: \
         write `presets = [\"claude-code\"]` (set in: airlock.toml)",
        "* `packs.python` must be a table (set in: airlock.toml)",
        "* `packs.rust` needs `version` (set in: airlock.toml): 1",
        "* `packs.sample.args.mode` must be a string (one of: fast, slow, or any other \
         non-empty string) (set in: airlock.toml)",
        "* `packs.sample.args.network` must be true or false",
        "* `packs.sample.args.model` unknown arg of version \"1\" (known: mode, network) \
         (set in: airlock.toml)",
        "* `packs.nodejs.enabled` must be true or false (set in: airlock.toml)",
        "* `packs.nodejs.node-version` unknown key (known: version, enabled, args; args go in \
         `args = { node-version = … }`) (set in: airlock.toml)",
        "* `packs.docker.args.node-version` unknown arg of version \"1\" (it has no args)",
        "* `packs.mise`: version \"legacy\" is not supported (supported: \"1\") \
         (set in: airlock.toml)",
        "* `packs.claude`: version \"legacy\" is not supported; use the list form \
         `presets = [\"claude-code\"]` (set in: airlock.toml)",
        "* `packs.codex`: version \"2\" is not supported (supported: \"1\")",
        "* `packs.git.version` must be 1 or higher (set in: airlock.toml)",
        "* `packs.copilot.version` must be a string or a whole number, not 1.5",
        "* `packs.alpine.version` must be a string or a whole number, not true",
        "* `packs.debian.version` must be 1 or higher",
        "* `packs.debian.args` must be a table (set in: airlock.toml)",
    ] {
        assert!(err.lines().any(|l| l.starts_with(line)), "{line}\n{err}");
    }

    assert_eq!(
        project_toml_error("packs = 1\n"),
        "invalid configuration\n* `packs` must be a table, not 1 (set in: airlock.toml)"
    );
}

/// Test that a failure in the config script of a pack is a config error
/// that names the pack and shows its hint.
///   1. Resolve the sample pack with the mode that its script refuses
///   2. Check the full error text
#[test]
fn pack_config_failure_is_error_with_pack_hint() {
    assert_eq!(
        project_toml_error("[packs]\nsample = { version = 1, args = { mode = \"broken\" } }\n"),
        "invalid configuration\n* pack sample: mode `broken` is not supported; use \
         `mode = \"fast\"`"
    );
}

/// Test that the error for a bad arg names the file that sets the arg, not
/// the file that sets the version.
///   1. Set the version in the local file and a bad arg in `airlock.toml`
///   2. Check that the error names `airlock.toml`
#[test]
fn arg_value_is_checked_in_file_that_sets_it() {
    let dirs = ConfigDirs::new();
    dirs.project_file(
        ".airlock/airlock.toml",
        "[packs]\nsample = { version = 1 }\n",
    )
    .project_file(
        "airlock.toml",
        "[packs]\nsample = { args = { mode = [\"fast\"] } }\n",
    );
    let err = dirs.error();
    assert!(
        err.contains(&format!(
            "* `packs.sample.args.mode` must be a string (one of: fast, slow, or any other \
             non-empty string) (set in: {})",
            dirs.project_origin("airlock.toml")
        )),
        "{err}"
    );
}

/// Test that two distro packs are a conflict, because both set the VM image.
///   1. Resolve the alpine and debian packs together
///   2. Check that the error names both packs and both images
#[test]
fn two_distro_packs_conflict_on_image() {
    assert_eq!(
        project_toml_error("[packs]\nalpine = { version = 1 }\ndebian = { version = 1 }\n"),
        "invalid configuration\n* packs alpine and debian both set `vm.image` \
         (\"alpine:latest\" vs \"debian:stable-slim\")"
    );
}
