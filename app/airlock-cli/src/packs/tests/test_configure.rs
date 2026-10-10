//! Tests of how a project config configures a pack: arg defaults, the
//! config that the pack adds, and the install fingerprint.

use std::collections::BTreeMap;

use crate::packs::{ArgValue, PackManager};
use crate::test_cfg::packs::{load_packs, resolve_with, test_pack_files, test_packs};
use crate::test_cfg::resolve_project_toml;

/// A text arg value.
fn text(value: &str) -> ArgValue {
    ArgValue::Text(value.into())
}

/// The install fingerprint of the first configured pack in `toml`.
fn fingerprint(packs: &PackManager, toml: &str) -> String {
    let resolved = resolve_with(packs, toml).unwrap();
    resolved.packs[0].setup_installer().unwrap().fingerprint
}

/// Test that a pack with a `config.lua` gets default values for the args
/// that the config does not set, and that its config follows the args.
///   1. Configure the sample pack with one arg and check that the other arg
///      gets its default
///   2. Check that the env, network rule and mount of the pack are in the
///      resolved config
///   3. Turn the network arg off and check that the network rule is not
///      there and that the mode env has its default value
#[test]
fn configuring_lua_pack_from_config_text_fills_defaults_and_applies_its_config() {
    let resolved =
        resolve_project_toml("[packs]\nsample = { version = 1, args = { mode = \"turbo\" } }\n")
            .unwrap();
    let sample = &resolved.packs[0];
    assert_eq!(
        sample.args(),
        &BTreeMap::from([
            ("mode".to_string(), text("turbo")),
            ("network".to_string(), ArgValue::Bool(true)),
        ])
    );
    assert_eq!(sample.non_default_args(), [("mode", &text("turbo"))]);
    let data = crate::test_cfg::temp_dir();
    let values = sample.config_values(data.path()).unwrap();
    assert_eq!(
        values["mounts"]["sample-dir"]["source"],
        "~/.airlock/sample"
    );
    assert_eq!(resolved.values.env["SAMPLE_MODE"].value, "turbo");
    assert!(resolved.values.network.rules.contains_key("sample"));
    assert!(resolved.values.mounts.contains_key("sample-dir"));

    let resolved =
        resolve_project_toml("[packs]\nsample = { version = 1, args = { network = false } }\n")
            .unwrap();
    assert!(!resolved.values.network.rules.contains_key("sample"));
    assert_eq!(resolved.values.env["SAMPLE_MODE"].value, "fast");
}

/// Test that a new arg with a default value keeps the install fingerprint.
/// Such an arg is not a breaking change, so existing sandboxes must not
/// see the pack as changed.
///   1. Get the fingerprint of the test pack with a non-default arg
///   2. Add a new bool arg with the default false to the same version
///   3. Check that the fingerprint stays the same
#[test]
fn new_arg_with_default_keeps_install_fingerprint() {
    let toml = "[packs]\nalpha = { version = 1, args = { mode = \"slow\" } }\n";
    let before = fingerprint(&test_packs(), toml);

    let mut files = test_pack_files();
    for (path, text) in &mut files {
        if *path == "alpha@1/pack.toml" {
            text.push_str(
                "\n[[args]]\nkey = \"extra\"\ntype = \"bool\"\n\
                 description = \"Extra\"\ndefault = false\n",
            );
        }
    }
    assert_eq!(fingerprint(&load_packs(files), toml), before);
}

/// Test that the install fingerprint changes with the pack name, version
/// and args, but not with the setup script text. A changed fingerprint
/// makes a new sandbox necessary, so it must not change without reason.
///   1. Check that explicit default args give the same fingerprint as no
///      args
///   2. Check that a different version, pack or arg value gives a new
///      fingerprint
///   3. Check that the order of the args has no effect
///   4. Change the setup script and check that the fingerprint stays the
///      same
#[test]
fn install_fingerprint_follows_name_version_and_args_but_not_script() {
    let packs = test_packs();
    let base = fingerprint(&packs, "[packs]\nalpha = { version = 1 }\n");
    // A SHA-256 digest in hex.
    assert_eq!(base.len(), 64);
    // The version as a string and the default arg values must not change
    // the fingerprint.
    for same in [
        "[packs]\nalpha = { version = 1, args = { mode = \"fast\" } }\n",
        "[packs]\nalpha = { version = \"1\", args = { fast-path = false, mode = \"fast\" } }\n",
    ] {
        assert_eq!(fingerprint(&packs, same), base, "{same}");
    }
    let mut seen = vec![base.clone()];
    for changed in [
        "[packs]\nalpha = { version = 2 }\n",
        "[packs]\nbeta = { version = 1 }\n",
        "[packs]\nalpha = { version = 1, args = { mode = \"slow\" } }\n",
        "[packs]\nalpha = { version = 1, args = { fast-path = true } }\n",
    ] {
        let fingerprint = fingerprint(&packs, changed);
        assert!(!seen.contains(&fingerprint), "{changed}");
        seen.push(fingerprint);
    }
    assert_eq!(
        fingerprint(
            &packs,
            "[packs]\nalpha = { version = 1, args = { fast-path = true, mode = \"slow\" } }\n"
        ),
        fingerprint(
            &packs,
            "[packs]\nalpha = { version = 1, args = { mode = \"slow\", fast-path = true } }\n"
        )
    );

    let mut files = test_pack_files();
    for (path, text) in &mut files {
        if *path == "alpha@1/setup.sh" {
            *text = "echo other\n".to_string();
        }
    }
    let other_script = load_packs(files);
    assert_eq!(
        fingerprint(&other_script, "[packs]\nalpha = { version = 1 }\n"),
        base
    );
}
