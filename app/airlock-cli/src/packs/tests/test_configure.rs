use std::collections::BTreeMap;

use crate::packs::{ArgValue, PackManager};
use crate::test_cfg::packs::{load_packs, resolve_with, test_pack_files, test_packs};
use crate::test_cfg::resolve_project_toml;

fn text(value: &str) -> ArgValue {
    ArgValue::Text(value.into())
}

fn fingerprint(packs: &PackManager, toml: &str) -> String {
    let resolved = resolve_with(packs, toml).unwrap();
    resolved.packs[0].setup_installer().unwrap().fingerprint
}

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
    let values = sample.config_values().unwrap();
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

#[test]
fn install_fingerprint_follows_name_version_and_args_but_not_script() {
    let packs = test_packs();
    let base = fingerprint(&packs, "[packs]\nalpha = { version = 1 }\n");
    assert_eq!(base.len(), 64);
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
