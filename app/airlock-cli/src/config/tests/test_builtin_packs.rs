use std::collections::{BTreeMap, HashSet};

use crate::config::config_values;
use crate::test_cfg::{configured_variants, resolve_project_toml};

/// The config values of every built-in pack with every combination of
/// its arg values: `(pack name, label, value)`.
fn builtin_values() -> Vec<(String, String, serde_json::Value)> {
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

/// The leaf paths of a document (`/a/b/c`), with arrays as leaves.
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

#[test]
fn builtin_packs_have_unique_names_versions_and_valid_arg_keys() {
    let mut seen = HashSet::new();
    for pack in crate::packs::init().unwrap().builtin() {
        let metadata = pack.metadata();
        assert!(
            seen.insert(metadata.name.clone()),
            "duplicate {}",
            metadata.name
        );
        assert!(!metadata.label.is_empty());
        assert!(
            metadata.version.parse::<u64>().is_ok_and(|v| v >= 1),
            "{}@{}",
            metadata.name,
            metadata.version
        );
        for arg in pack.args() {
            let key = &arg.key;
            let mut chars = key.chars();
            assert!(
                chars.next().is_some_and(|c| c.is_ascii_lowercase())
                    && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{}.{key}",
                metadata.name
            );
            assert!(key != "enabled" && key != "version" && key != "args");
        }
    }
}

#[test]
fn every_builtin_pack_variant_is_valid_config() {
    for (_, label, value) in builtin_values() {
        config_values::parse(value).unwrap_or_else(|e| panic!("pack `{label}` fails: {e}"));
    }
}

#[test]
fn builtin_pack_documents_merge_same_in_any_order() {
    let distros: Vec<String> = crate::packs::init()
        .unwrap()
        .builtin()
        .iter()
        .filter(|p| p.metadata().kind == crate::packs::PackKind::Distro)
        .map(|p| p.metadata().name.clone())
        .collect();
    let documents: Vec<(String, String, BTreeMap<String, serde_json::Value>)> = builtin_values()
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
fn docker_preset_runs_unhardened_daemon_and_allows_registries() {
    let config = resolve_project_toml("presets = [\"docker\"]\n")
        .unwrap()
        .values;
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
fn acp_arg_points_adapter_at_agent_binary() {
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

#[test]
fn agent_packs_enable_their_service_unless_project_turns_it_off() {
    let claude = resolve_project_toml("[packs]\nclaude = { version = 1 }\n").unwrap();
    let net = &claude.values.network;
    assert_eq!(net.services.get("anthropic"), Some(&true));
    assert_eq!(
        net.rules["claude-code"].allow,
        ["claude.ai:443", "downloads.claude.ai:443"]
    );
    crate::network::check_passthrough(net).unwrap();

    let codex = resolve_project_toml("[packs]\ncodex = { version = 1 }\n").unwrap();
    let net = &codex.values.network;
    assert_eq!(net.services.get("openai"), Some(&true));
    assert_eq!(net.rules["codex"].allow, ["api.openai.com:443"]);
    crate::network::check_passthrough(net).unwrap();

    let off = resolve_project_toml(
        "[packs]\nclaude = { version = 1 }\n[network.services]\nanthropic = false\n",
    )
    .unwrap();
    assert!(crate::services::enabled(&off.values.network.services).is_empty());
}
