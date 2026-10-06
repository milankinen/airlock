use std::collections::BTreeMap;

use include_dir::{Dir, include_dir};

use crate::config::config_values::{self, ConfigValues};
use crate::config::legacy_presets::take_presets_key;
use crate::config::merge::{merge_json, normalize_env};
use crate::test_support::{configured_variants, resolve_project_toml};

static FIXTURES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/config/tests/fixtures");

/// The fixture document `name` (`fixtures/<name>.toml`).
fn fixture_document(name: &str) -> serde_json::Value {
    let file = FIXTURES
        .files()
        .find(|f| f.path().file_stem().is_some_and(|s| s == name))
        .unwrap_or_else(|| panic!("no fixture {name}"));
    let toml_str = std::str::from_utf8(file.contents()).unwrap();
    toml::from_str(toml_str).unwrap()
}

fn json(toml_str: &str) -> serde_json::Value {
    toml::from_str(toml_str).unwrap()
}

fn empty() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

/// Merge `documents` onto `base` in order, each env-normalized (as the
/// documents of a preset merge).
fn apply_documents(
    mut base: serde_json::Value,
    documents: Vec<serde_json::Value>,
) -> serde_json::Value {
    for mut document in documents {
        normalize_env(&mut document);
        base = merge_json(base, document);
    }
    base
}

/// Apply the fixture documents that `config` lists, then `config` on top.
fn resolve(mut config: serde_json::Value) -> serde_json::Value {
    let names = take_presets_key(&mut config, "test")
        .unwrap()
        .unwrap_or_default();
    let documents = names.iter().map(|n| fixture_document(n)).collect();
    merge_json(apply_documents(empty(), documents), config)
}

// -- take_presets_key ------------------------------------------------

fn take(value: serde_json::Value) -> anyhow::Result<Option<Vec<String>>> {
    let mut value = value;
    let taken = take_presets_key(&mut value, "airlock.toml");
    assert!(value.get("presets").is_none(), "the key is removed");
    taken
}

#[test]
fn take_returns_the_list_and_strips_the_key() {
    assert_eq!(
        take(json(r#"presets = ["a", "b"]"#)).unwrap(),
        Some(vec!["a".to_string(), "b".to_string()])
    );
}

#[test]
fn take_returns_none_when_missing() {
    assert_eq!(take(json("cpus = 4")).unwrap(), None);
}

#[test]
fn take_rejects_null_and_other_types() {
    for (value, text) in [
        (serde_json::json!({ "presets": null }), "`presets` is empty"),
        (
            serde_json::json!({ "presets": 1 }),
            "must be a list of preset names",
        ),
        (
            serde_json::json!({ "presets": true }),
            "must be a list of preset names",
        ),
        (
            serde_json::json!({ "presets": "python" }),
            "must be a list of preset names",
        ),
        (
            serde_json::json!({ "presets": { "a": 1 } }),
            "use a [packs] table",
        ),
        (
            serde_json::json!({ "presets": ["python", 1] }),
            "list entries must be preset names",
        ),
    ] {
        let e = take(value).unwrap_err().to_string();
        assert!(e.starts_with("airlock.toml: "), "{e}");
        assert!(e.contains(text), "{e}");
    }
}

// -- merging documents -----------------------------------------------

#[test]
fn no_presets_passes_through() {
    let result = resolve(json("cpus = 4"));
    assert_eq!(result["cpus"], 4);
}

#[test]
fn single_preset_applied_as_base() {
    // test-base sets image="test:base", cpus=2
    // user overrides cpus=16
    let result = resolve(json(
        r#"
        presets = ["test-base"]
        cpus = 16
    "#,
    ));
    assert_eq!(result["image"], "test:base"); // from preset
    assert_eq!(result["cpus"], 16); // user wins
}

#[test]
fn multiple_presets_applied_in_order() {
    // test-base: cpus=2
    // test-overlay: cpus=8, memory="1 GB"
    let result = resolve(json(
        r#"
        presets = ["test-base", "test-overlay"]
    "#,
    ));
    assert_eq!(result["image"], "test:base"); // from test-base
    assert_eq!(result["cpus"], 8); // test-overlay overrides test-base
    assert_eq!(result["memory"], "1 GB"); // from test-overlay
}

#[test]
fn user_config_overrides_presets() {
    let result = resolve(json(
        r#"
        presets = ["test-overlay"]
        cpus = 1
    "#,
    ));
    assert_eq!(result["cpus"], 1); // user wins over preset's 8
    assert_eq!(result["memory"], "1 GB"); // preset value kept
}

/// Plain-string `[env]` entries of a document merge field-wise.
#[test]
fn document_env_is_normalized() {
    let masked = json("[env]\nTOKEN = { value = \"a\", mask = true }\n");
    let plain = json("[env]\nTOKEN = \"b\"\n");
    let result = apply_documents(empty(), vec![masked, plain]);
    assert_eq!(
        result["env"]["TOKEN"],
        serde_json::json!({"value": "b", "mask": true})
    );
}

#[test]
fn presets_key_stripped_from_final_output() {
    let result = resolve(json(
        r#"
        presets = ["test-base"]
        cpus = 4
    "#,
    ));
    assert!(result.get("presets").is_none());
}

#[test]
fn full_parse_with_preset() {
    config_values::parse(resolve(json(r#"presets = ["test-base"]"#))).unwrap();
}

// -- bundled packs --------------------------------------------------

/// The config values of every built-in pack with every combination of
/// its arg values, with the pack name and the args.
fn bundled_values() -> Vec<(String, String, serde_json::Value)> {
    let mut values = Vec::new();
    for pack in crate::packs::init().unwrap().builtin() {
        for configured in configured_variants(&pack) {
            let label = format!(
                "{}@{} {:?}",
                pack.metadata().name,
                pack.metadata().version,
                configured.args()
            );
            values.push((
                pack.metadata().name.clone(),
                label,
                configured.config_values().unwrap(),
            ));
        }
    }
    values
}

/// Each pack alone is a valid config. (The loader checks that a
/// document sets no `packs`.)
#[test]
fn all_bundled_packs_are_valid() {
    for (_, label, value) in bundled_values() {
        config_values::parse(value)
            .unwrap_or_else(|e| panic!("pack `{label}` fails to parse: {e}"));
    }
}

/// `preset` is a released list name: the legacy documents still mirror
/// the bundled pack's config (as released).
fn parse_bundled(preset: &str) -> ConfigValues {
    resolve_project_toml(&format!("presets = [\"{preset}\"]\n"))
        .unwrap()
        .values
}

/// The leaf paths of a document (`a.b.c`), with arrays as leaves.
fn leaves(prefix: &str, value: &serde_json::Value, out: &mut BTreeMap<String, serde_json::Value>) {
    match value {
        serde_json::Value::Object(map) => {
            for (k, v) in map {
                leaves(&format!("{prefix}/{k}"), v, out);
            }
        }
        other => {
            out.insert(prefix.to_string(), other.clone());
        }
    }
}

/// The packs apply in pack order instead of list order. That is the
/// same config because no two packs set the same path, except a scalar
/// with the same value, and no path of one is inside a value of another.
/// Arrays (which concatenate) are never shared. The versions of one
/// pack hold copies of the same documents. The distro packs all set the
/// image: a config has at most one of them.
#[test]
fn bundled_documents_are_order_independent() {
    let distros: Vec<String> = crate::packs::init()
        .unwrap()
        .builtin()
        .iter()
        .filter(|p| p.metadata().kind == crate::packs::PackKind::Distro)
        .map(|p| p.metadata().name.clone())
        .collect();
    let documents: Vec<(String, String, BTreeMap<String, serde_json::Value>)> = bundled_values()
        .into_iter()
        .map(|(pack, label, value)| {
            let mut paths = BTreeMap::new();
            leaves("", &value, &mut paths);
            (pack, label, paths)
        })
        .collect();
    for (i, (a_pack, a, a_paths)) in documents.iter().enumerate() {
        for (b_pack, b, b_paths) in &documents[i + 1..] {
            if a_pack == b_pack || (distros.contains(a_pack) && distros.contains(b_pack)) {
                continue;
            }
            for (path, a_value) in a_paths {
                if let Some(b_value) = b_paths.get(path) {
                    assert!(
                        !a_value.is_array() && a_value == b_value,
                        "{a} and {b} both set {path}"
                    );
                }
                let inside = format!("{path}/");
                assert!(
                    !b_paths.keys().any(|p| p.starts_with(&inside)),
                    "{b} sets a path inside {a}'s {path}"
                );
            }
            for path in b_paths.keys() {
                let inside = format!("{path}/");
                assert!(
                    !a_paths.keys().any(|p| p.starts_with(&inside)),
                    "{a} sets a path inside {b}'s {path}"
                );
            }
        }
    }
}

#[test]
fn docker_runs_unhardened_daemon_and_allows_registries() {
    let config = parse_bundled("docker");
    let daemon = &config.daemons["dockerd"];
    assert!(!daemon.harden);
    assert_eq!(daemon.timeout, 10);
    assert!(daemon.command.last().unwrap().ends_with("exec dockerd"));
    let allow = &config.network.rules["docker-registries"].allow;
    for host in [
        "registry-1.docker.io",
        "auth.docker.io",
        "production.cloudfront.docker.com",
        "production.cloudflare.docker.com",
        "docker-images-prod.6aa30f8b08e16409b46e0173d6de2f56.r2.cloudflarestorage.com",
        "ghcr.io",
        "pkg-containers.githubusercontent.com",
    ] {
        assert!(allow.contains(&host.to_string()), "missing {host}");
    }
}

#[test]
fn python_points_tls_at_system_bundle() {
    let config = parse_bundled("python");
    for key in ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "PIP_CERT"] {
        let var = &config.env[key];
        assert_eq!(var.value, "/etc/ssl/certs/ca-certificates.crt", "{key}");
        assert!(!var.mask, "{key}");
    }
}

/// With the `acp` arg, the agent packs point the ACP adapter at the
/// agent binary of the setup script; without it, the env has no path.
#[test]
fn the_acp_arg_points_the_adapter_at_the_agent_binary() {
    let env = |pack: &str, acp: bool| {
        resolve_project_toml(&format!(
            "[packs]\n{pack} = {{ version = 1, args = {{ acp = {acp} }} }}\n"
        ))
        .unwrap()
        .values
        .env
    };
    assert_eq!(
        env("claude", true)["CLAUDE_CODE_EXECUTABLE"].value,
        "/usr/local/bin/claude"
    );
    assert!(!env("claude", false).contains_key("CLAUDE_CODE_EXECUTABLE"));
    assert_eq!(
        env("codex", true)["CODEX_PATH"].value,
        "/usr/local/bin/codex"
    );
    assert!(!env("codex", false).contains_key("CODEX_PATH"));
}
