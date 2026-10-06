//! Golden oracle for the released `presets = [..]` list form.
//!
//! Every case builds [`LayeredConfig`] from [`Layer::new`] values and records the
//! result of `.resolve(&presets)`: `serde_json::to_value(&config.values)` or
//! the error text. The results are compared by `Value` equality with
//! `golden/legacy-presets.json`, which was generated from the code before
//! presets got versions and must stay unchanged.
//!
//! Regenerate the file only for a deliberate preset document edit:
//!
//! ```sh
//! mise x -- cargo test -p airlock-cli regenerate_legacy_presets_golden -- --ignored
//! ```

use std::path::Path;

use serde_json::{Map, Value, json};

use crate::config::files::parse_file;
use crate::config::{Layer, LayeredConfig};
use crate::test_support::block_on;

const GOLDEN: &str = include_str!("golden/legacy-presets.json");
const GOLDEN_PATH: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/src/config/tests/golden/legacy-presets.json"
);

/// The released preset names (11, plus the later addition `docker`), frozen.
const NAMES: [&str; 12] = [
    "alpine",
    "arch",
    "claude-code",
    "copilot-cli",
    "debian",
    "docker",
    "fedora",
    "nodejs",
    "openai-codex",
    "python",
    "rust",
    "suse",
];

/// [`NAMES`] without the later addition `docker`: the 11 names released on
/// `main`, so these cases double as a main-equivalence check.
const NAMES_NO_DOCKER: [&str; 11] = [
    "alpine",
    "arch",
    "claude-code",
    "copilot-cli",
    "debian",
    "fedora",
    "nodejs",
    "openai-codex",
    "python",
    "rust",
    "suse",
];

/// The config slot of one layer.
#[derive(Clone, Copy)]
enum Slot {
    User,
    Local,
    Project,
}

/// One golden case: its layers, lowest precedence first within a slot.
struct Case {
    name: String,
    layers: Vec<(Slot, String, Value)>,
}

impl Case {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            layers: vec![],
        }
    }

    fn layer(mut self, slot: Slot, origin: &str, value: Value) -> Self {
        self.layers.push((slot, origin.to_string(), value));
        self
    }

    fn user(self, value: Value) -> Self {
        self.layer(Slot::User, "~/.airlock/config.toml", value)
    }

    fn local(self, value: Value) -> Self {
        self.layer(Slot::Local, ".airlock/airlock.toml", value)
    }

    fn project(self, value: Value) -> Self {
        self.layer(Slot::Project, "airlock.toml", value)
    }

    fn project_local(self, value: Value) -> Self {
        self.layer(Slot::Project, "airlock.local.toml", value)
    }

    /// Parse `text` as the file `origin` (its extension picks the format).
    fn file(self, slot: Slot, origin: &str, text: &str) -> Self {
        let value = parse_file(Path::new(origin), text)
            .unwrap_or_else(|e| panic!("case {}: {e:#}", self.name));
        self.layer(slot, origin, value)
    }

    /// Resolve the case and record the config or the error text.
    ///
    /// The lowest user layer pins `vm.cpus` and `vm.memory`, whose defaults
    /// depend on the host.
    fn record(&self) -> Value {
        let resolve = |layers: LayeredConfig| {
            block_on(layers.resolve(
                &crate::packs::init()?,
                &crate::config::ConfigOverrides::default(),
            ))
            .map(|resolved| resolved.values)
        };
        match self.layered_config().and_then(resolve) {
            Ok(config) => json!({ "config": serde_json::to_value(&config).unwrap() }),
            Err(e) => json!({ "error": format!("{e:#}") }),
        }
    }

    /// The layers of the case, as loaded.
    fn layered_config(&self) -> anyhow::Result<LayeredConfig> {
        let pinned = Layer::new(
            "~/.airlock/airlock.toml",
            json!({ "vm": { "cpus": 2, "memory": "2 GB" } }),
        )?;
        let mut user = vec![pinned];
        let mut local = None;
        let mut project = vec![];
        for (slot, origin, value) in &self.layers {
            let layer = Layer::new(origin.clone(), value.clone())?;
            match slot {
                Slot::User => user.push(layer),
                Slot::Local => {
                    assert!(local.is_none(), "case {}: two local layers", self.name);
                    local = Some(layer);
                }
                Slot::Project => project.push(layer),
            }
        }
        Ok(LayeredConfig::new(user, local, project))
    }
}

fn presets(names: &[&str]) -> Value {
    json!({ "presets": names })
}

fn cases() -> Vec<Case> {
    let mut cases = vec![];

    // Each name alone, every ordered pair, and all names together.
    for a in NAMES {
        cases.push(Case::new(format!("alone/{a}")).project(presets(&[a])));
    }
    for a in NAMES {
        for b in NAMES {
            cases.push(Case::new(format!("pair/{a}+{b}")).project(presets(&[a, b])));
        }
    }
    let mut reversed = NAMES;
    reversed.reverse();
    cases.push(Case::new("all/catalog-order").project(presets(&NAMES)));
    cases.push(Case::new("all/reversed").project(presets(&reversed)));
    cases.push(
        Case::new("all/one-per-layer")
            .user(presets(&NAMES[..5]))
            .local(presets(&NAMES[5..10]))
            .project(presets(&NAMES[10..])),
    );

    // The same, without `docker`: these cases resolve exactly as on `main`.
    let mut reversed_no_docker = NAMES_NO_DOCKER;
    reversed_no_docker.reverse();
    cases.push(Case::new("all/no-docker/catalog-order").project(presets(&NAMES_NO_DOCKER)));
    cases.push(Case::new("all/no-docker/reversed").project(presets(&reversed_no_docker)));
    cases.push(
        Case::new("all/no-docker/one-per-layer")
            .user(presets(&NAMES_NO_DOCKER[..4]))
            .local(presets(&NAMES_NO_DOCKER[4..8]))
            .project(presets(&NAMES_NO_DOCKER[8..])),
    );

    cases.extend(text_cases());
    cases.extend(split_cases());
    cases.extend(overlap_cases());
    cases
}

/// Configs from the docs, the examples, and the bats tests, as TOML text,
/// and the same configs converted to JSON and YAML text.
fn text_cases() -> Vec<Case> {
    const TEXTS: &[(&str, &str)] = &[
        (
            "manual-using-presets",
            r#"presets = ["debian", "rust", "claude-code"]

[vm]
image = "ubuntu:24.04"
"#,
        ),
        (
            "manual-combining",
            r#"presets = ["debian", "python", "claude-code"]

[vm]
image = "ubuntu:24.04"

[network]
policy = "deny-by-default"

[network.rules.internal-api]
allow = ["api.internal.company.com:443"]

[network.middleware.internal-api-auth]
target = ["api.internal.company.com:443"]
env.TOKEN = "${INTERNAL_API_TOKEN}"
script = '''
req:setHeader("Authorization", "Bearer " .. env.TOKEN)
'''
"#,
        ),
        (
            "manual-configuration-rust",
            r#"presets = ["rust"]

[vm]
image = "ubuntu:24.04"
cpus = 4
memory = "4 GB"
"#,
        ),
        (
            "manual-openai-codex",
            r#"presets = ["openai-codex"]

[network]
policy = "deny-by-default"

[vm]
image = "docker/sandbox-templates:codex-docker"
"#,
        ),
        (
            "manual-claude-code",
            r#"presets = ["claude-code"]

[network]
policy = "deny-by-default"

[vm]
image = "docker/sandbox-templates:claude-code"
"#,
        ),
        (
            "manual-copilot-cli",
            r#"presets = ["copilot-cli"]

[network]
policy = "deny-by-default"
"#,
        ),
        (
            "manual-docker",
            r#"presets = ["docker"]
"#,
        ),
        (
            "example-docker",
            r#"presets = ["python", "docker"]

[vm]
image = "airlock-example:docker"

[network.ports.app]
guest = [8000]
"#,
        ),
        (
            "bats-debian",
            r#"presets = ["debian"]
"#,
        ),
        (
            "bats-rust",
            r#"presets = ["rust"]
"#,
        ),
        (
            "bats-multiple",
            r#"presets = ["debian", "rust", "claude-code"]
"#,
        ),
        (
            "bats-start-claude-code",
            r#"presets = ["claude-code"]

[vm]
image = "airlock-test.invalid/no-such-image:1"
"#,
        ),
    ];

    let mut cases = vec![];
    for (name, toml_text) in TEXTS {
        let value: Value = toml::from_str(toml_text).unwrap();
        let json_text = serde_json::to_string_pretty(&value).unwrap();
        let yaml_text = serde_yaml::to_string(&value).unwrap();
        cases.push(Case::new(format!("toml/{name}")).file(
            Slot::Project,
            "airlock.toml",
            toml_text,
        ));
        cases.push(Case::new(format!("json/{name}")).file(
            Slot::Project,
            "airlock.json",
            &json_text,
        ));
        cases.push(Case::new(format!("yaml/{name}")).file(
            Slot::Project,
            "airlock.yaml",
            &yaml_text,
        ));
    }

    cases.push(
        Case::new("mixed/yml-project-local")
            .file(Slot::Project, "airlock.toml", "presets = [\"rust\"]\n")
            .file(
                Slot::Project,
                "airlock.local.yml",
                "presets: [rust, nodejs]\nnetwork:\n  rules:\n    rust-packages:\n      allow: [\"mirror.example.com\"]\n",
            ),
    );
    cases
}

/// Lists split over user, local, and project files, and the same name in
/// two layers.
fn split_cases() -> Vec<Case> {
    vec![
        Case::new("split/user-project")
            .user(presets(&["claude-code"]))
            .project(presets(&["python"])),
        Case::new("split/user-local-project")
            .user(presets(&["debian"]))
            .local(presets(&["nodejs"]))
            .project(presets(&["claude-code"])),
        Case::new("split/local-project")
            .local(presets(&["openai-codex"]))
            .project(presets(&["rust", "docker"])),
        Case::new("split/project-and-project-local")
            .project(presets(&["alpine", "python"]))
            .project_local(presets(&["copilot-cli"])),
        Case::new("split/two-user-files")
            .layer(Slot::User, "~/.airlock/config.toml", presets(&["fedora"]))
            .layer(Slot::User, "~/.airlock.toml", presets(&["suse", "arch"])),
        Case::new("same/user-project")
            .user(presets(&["claude-code"]))
            .project(presets(&["claude-code"])),
        Case::new("same/local-project")
            .local(presets(&["openai-codex"]))
            .project(presets(&["openai-codex"])),
        Case::new("same/all-layers")
            .user(presets(&["python", "rust"]))
            .local(presets(&["rust"]))
            .project(presets(&["python"]))
            .project_local(presets(&["rust", "python"])),
        Case::new("same/twice-in-one-list").project(presets(&["docker", "nodejs", "docker"])),
        Case::new("empty/list").project(presets(&[])),
        Case::new("empty/no-presets").project(json!({})),
    ]
}

/// Layer bodies that overlap keys of the preset documents.
fn overlap_cases() -> Vec<Case> {
    vec![
        Case::new("overlap/plain-token-over-masked-local")
            .project(presets(&["claude-code"]))
            .local(json!({ "env": { "CLAUDE_CODE_OAUTH_TOKEN": "${OTHER_TOKEN}" } })),
        Case::new("overlap/plain-token-over-masked-same-file").project(json!({
            "presets": ["claude-code"],
            "env": { "CLAUDE_CODE_OAUTH_TOKEN": "${OTHER_TOKEN}", "IS_SANDBOX": "0" },
        })),
        Case::new("overlap/plain-token-user-below-preset-project")
            .user(json!({ "env": { "CLAUDE_CODE_OAUTH_TOKEN": "${USER_TOKEN}" } }))
            .project(presets(&["claude-code"])),
        Case::new("overlap/claude-rule-disabled")
            .project(presets(&["claude-code"]))
            .project_local(json!({
                "network": { "rules": { "claude-code": { "enabled": false } } },
            })),
        Case::new("overlap/alpine-rule-disabled")
            .project(presets(&["alpine", "python"]))
            .project_local(json!({
                "network": { "rules": { "alpine-packages": { "enabled": false } } },
            })),
        Case::new("overlap/python-allow-concatenates").project(json!({
            "presets": ["python"],
            "network": { "rules": { "python-packages": { "allow": ["pypi.internal.example.com"] } } },
        })),
        Case::new("overlap/python-allow-concatenates-across-layers")
            .user(json!({
                "network": { "rules": { "python-packages": { "allow": ["user.example.com"] } } },
            }))
            .project(json!({
                "presets": ["python"],
                "network": { "rules": { "python-packages": { "allow": ["project.example.com"] } } },
            })),
        Case::new("overlap/codex-dir-mount").project(json!({
            "presets": ["openai-codex"],
            "mounts": { "codex-dir": { "source": "~/work/codex", "missing": "fail" } },
        })),
        Case::new("overlap/node-ca-shared")
            .project(presets(&["claude-code", "nodejs"]))
            .project_local(json!({ "env": { "NODE_EXTRA_CA_CERTS": "/custom/ca.pem" } })),
        Case::new("overlap/docker-daemon-timeout").project(json!({
            "presets": ["docker"],
            "daemons": { "dockerd": { "timeout": 30 } },
        })),
        Case::new("overlap/codex-inject-extra-host").project(json!({
            "presets": ["openai-codex"],
            "network": { "rules": { "codex": { "allow": ["proxy.openai.example.com:443"] } } },
        })),
    ]
}

fn record_all() -> Map<String, Value> {
    let mut results = Map::new();
    for case in cases() {
        let result = case.record();
        assert!(
            results.insert(case.name.clone(), result).is_none(),
            "duplicate case {}",
            case.name
        );
    }
    results
}

/// One case per line, sorted by name, so a diff shows the changed cases.
fn render(results: &Map<String, Value>) -> String {
    let mut names: Vec<_> = results.keys().collect();
    names.sort();
    let lines: Vec<String> = names
        .into_iter()
        .map(|name| format!("  {}: {}", Value::from(name.as_str()), results[name]))
        .collect();
    format!("{{\n{}\n}}\n", lines.join(",\n"))
}

#[test]
fn legacy_presets_match_golden() {
    let golden: Map<String, Value> = serde_json::from_str(GOLDEN).unwrap();
    let actual = record_all();
    assert!(actual.len() >= 218, "only {} cases", actual.len());

    let mut problems = vec![];
    for (name, expected) in &golden {
        match actual.get(name) {
            None => problems.push(format!("{name}: missing from the harness")),
            Some(value) if value != expected => {
                problems.push(format!("{name}:\n  golden: {expected}\n  actual: {value}"));
            }
            Some(_) => {}
        }
    }
    for name in actual.keys() {
        if !golden.contains_key(name) {
            problems.push(format!("{name}: missing from the golden file"));
        }
    }
    assert!(
        problems.is_empty(),
        "{} of {} cases differ:\n{}",
        problems.len(),
        golden.len(),
        problems.join("\n")
    );
}

/// Every golden case is a valid config: the oracle covers what must keep
/// working, not errors.
#[test]
fn legacy_presets_golden_has_no_errors() {
    let golden: Map<String, Value> = serde_json::from_str(GOLDEN).unwrap();
    let errors: Vec<_> = golden
        .iter()
        .filter(|(_, v)| v.get("error").is_some())
        .map(|(name, v)| format!("{name}: {}", v["error"]))
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));
}

#[test]
#[ignore = "rewrites the golden file; run only for a deliberate preset document edit"]
fn regenerate_legacy_presets_golden() {
    std::fs::write(GOLDEN_PATH, render(&record_all())).unwrap();
}
