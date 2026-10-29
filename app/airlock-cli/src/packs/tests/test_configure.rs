//! Tests of how a project config configures a pack: arg defaults, the
//! config that the pack adds, and the install fingerprint.

use std::collections::BTreeMap;

use crate::packs::approval::Approved;
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
    let tmp = crate::test_cfg::temp_dir();
    let data = crate::test_cfg::oci::data_dir_at(tmp.path());
    let values = sample
        .config_values(&data, &Approved::for_test(sample.pack()))
        .unwrap()
        .0;
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

/// Test that the install fingerprints of the built-in packs do not change.
/// Existing sandboxes store these values. If a fingerprint changes, every
/// sandbox with the pack counts as changed and needs a new install. A
/// change of these values is breaking and needs a new pack version.
///   1. Configure each built-in pack alone, with default args
///   2. Configure python and docker with non-default args
///   3. Check the fingerprints against the stored values
#[test]
fn builtin_pack_fingerprints_with_current_inputs_stay_same() {
    let packs = crate::packs::init_builtin().unwrap();
    let mut actual: Vec<(String, String)> = Vec::new();
    for pack in packs.offered() {
        let name = &pack.metadata().id;
        let version = pack.metadata().version.as_deref().unwrap();
        let toml = format!("[packs]\n{name} = {{ version = {version} }}\n");
        let resolved = resolve_with(&packs, &toml).unwrap();
        // Packs without a setup script have no fingerprint.
        if let Some(installer) = resolved.packs[0].setup_installer() {
            actual.push((format!("{name}@{version}"), installer.fingerprint));
        }
    }
    for toml in [
        "[packs]\npython = { version = 1, args = { python-version = \"3.12\", allow-pypi = false } }\n",
        "[packs]\ndocker = { version = 1, args = { allow-pulls = false } }\n",
    ] {
        let name = toml.lines().nth(1).unwrap().split(' ').next().unwrap();
        actual.push((format!("{name} args"), fingerprint(&packs, toml)));
    }
    let actual: Vec<(&str, &str)> = actual
        .iter()
        .map(|(key, value)| (key.as_str(), value.as_str()))
        .collect();
    let expected: Vec<(&str, &str)> = vec![
        (
            "claude@1",
            "6f2bbd3021e675f80ce3f11af294c91d05c79d5d8700c44846215c35f9de64ae",
        ),
        (
            "codex@1",
            "57f26c9404cafb9bc34ce76acb30bd9ebeff949f1577cab1f3b10a13b73821ae",
        ),
        (
            "copilot@1",
            "4011a95d48ce53381c6c223da455f02c3ddd2a291bf8678130a22666586d7cab",
        ),
        (
            "docker@1",
            "d83a8a78db9ee4bde96fef23733203f2e79b570d7d6ee3f38e1a1b0a5c20c196",
        ),
        (
            "git@1",
            "53d949016705f158a3afbd5e85f572934ecb4c6e92f65a6387f355151f9b9537",
        ),
        (
            "mise@1",
            "37db4a1d89fdd9d11cad4482050f3a335d32a5aaf015da9abddce563b4a99269",
        ),
        (
            "nodejs@1",
            "42c0eb421a21fa953458775fc3112191897e2b411b17141bc83ebf3ee8c22858",
        ),
        (
            "python@1",
            "ea99c7d05a342803b7683b559b56495ea5766316252575c0846c164eaf5255b2",
        ),
        (
            "rust@1",
            "3e6894db53328e5f6df7afa87946ef32f519ad289a2187b4d13d2083ffef17d0",
        ),
        (
            "python args",
            "25580728efdbd1483f9c5274b8cd90e768e181b537757a6ed51df8f00fb6e485",
        ),
        (
            "docker args",
            "258549d437f8f3d8f902fa538217be9f44fe37b25e727305ffbe44ecba238b55",
        ),
    ];
    assert_eq!(actual, expected);
}
