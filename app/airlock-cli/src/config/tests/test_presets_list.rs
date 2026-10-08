use serde_json::{Map, Value, json};

use crate::test_cfg::{ConfigDirs, project_toml_error};

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
/// `main`.
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

/// One golden case: config files, each `(in home, relative path, text)`.
struct Case {
    name: String,
    files: Vec<(bool, String, String)>,
}

impl Case {
    fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            files: vec![],
        }
    }

    fn json_file(mut self, home: bool, rel: &str, value: &Value) -> Self {
        self.files
            .push((home, rel.to_string(), serde_json::to_string(value).unwrap()));
        self
    }

    fn user(self, value: &Value) -> Self {
        self.json_file(true, ".airlock/config.json", value)
    }

    fn user_at(self, rel: &str, value: &Value) -> Self {
        self.json_file(true, rel, value)
    }

    fn local(self, value: &Value) -> Self {
        self.json_file(false, ".airlock/airlock.json", value)
    }

    fn project(self, value: &Value) -> Self {
        self.json_file(false, "airlock.json", value)
    }

    fn project_local(self, value: &Value) -> Self {
        self.json_file(false, "airlock.local.json", value)
    }

    /// The project file `rel` with `text`.
    fn file(mut self, rel: &str, text: &str) -> Self {
        self.files.push((false, rel.to_string(), text.to_string()));
        self
    }

    /// Write the files, resolve them, and record the config or the error
    /// text. `~/.airlock/airlock.json` pins `vm.cpus` and `vm.memory`,
    /// whose defaults depend on the host.
    fn record(&self) -> Value {
        let dirs = ConfigDirs::new();
        dirs.user_file(
            ".airlock/airlock.json",
            r#"{"vm": {"cpus": 2, "memory": "2 GB"}}"#,
        );
        for (home, rel, text) in &self.files {
            if *home {
                dirs.user_file(rel, text);
            } else {
                dirs.project_file(rel, text);
            }
        }
        match dirs.resolve() {
            Ok(resolved) => json!({ "config": serde_json::to_value(&resolved.values).unwrap() }),
            Err(e) => json!({ "error": format!("{e:#}") }),
        }
    }
}

fn presets(names: &[&str]) -> Value {
    json!({ "presets": names })
}

fn cases() -> Vec<Case> {
    let mut cases = vec![];

    for a in NAMES {
        cases.push(Case::new(format!("alone/{a}")).project(&presets(&[a])));
    }
    for a in NAMES {
        for b in NAMES {
            cases.push(Case::new(format!("pair/{a}+{b}")).project(&presets(&[a, b])));
        }
    }
    let mut reversed = NAMES;
    reversed.reverse();
    cases.push(Case::new("all/catalog-order").project(&presets(&NAMES)));
    cases.push(Case::new("all/reversed").project(&presets(&reversed)));
    cases.push(
        Case::new("all/one-per-layer")
            .user(&presets(&NAMES[..5]))
            .local(&presets(&NAMES[5..10]))
            .project(&presets(&NAMES[10..])),
    );

    let mut reversed_no_docker = NAMES_NO_DOCKER;
    reversed_no_docker.reverse();
    cases.push(Case::new("all/no-docker/catalog-order").project(&presets(&NAMES_NO_DOCKER)));
    cases.push(Case::new("all/no-docker/reversed").project(&presets(&reversed_no_docker)));
    cases.push(
        Case::new("all/no-docker/one-per-layer")
            .user(&presets(&NAMES_NO_DOCKER[..4]))
            .local(&presets(&NAMES_NO_DOCKER[4..8]))
            .project(&presets(&NAMES_NO_DOCKER[8..])),
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
        cases.push(Case::new(format!("toml/{name}")).file("airlock.toml", toml_text));
        cases.push(Case::new(format!("json/{name}")).file("airlock.json", &json_text));
        cases.push(Case::new(format!("yaml/{name}")).file("airlock.yaml", &yaml_text));
    }

    cases.push(
        Case::new("mixed/yml-project-local")
            .file("airlock.toml", "presets = [\"rust\"]\n")
            .file("airlock.local.yml",
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
            .user(&presets(&["claude-code"]))
            .project(&presets(&["python"])),
        Case::new("split/user-local-project")
            .user(&presets(&["debian"]))
            .local(&presets(&["nodejs"]))
            .project(&presets(&["claude-code"])),
        Case::new("split/local-project")
            .local(&presets(&["openai-codex"]))
            .project(&presets(&["rust", "docker"])),
        Case::new("split/project-and-project-local")
            .project(&presets(&["alpine", "python"]))
            .project_local(&presets(&["copilot-cli"])),
        Case::new("split/two-user-files")
            .user_at(".airlock/config.json", &presets(&["fedora"]))
            .user_at(".airlock.json", &presets(&["suse", "arch"])),
        Case::new("same/user-project")
            .user(&presets(&["claude-code"]))
            .project(&presets(&["claude-code"])),
        Case::new("same/local-project")
            .local(&presets(&["openai-codex"]))
            .project(&presets(&["openai-codex"])),
        Case::new("same/all-layers")
            .user(&presets(&["python", "rust"]))
            .local(&presets(&["rust"]))
            .project(&presets(&["python"]))
            .project_local(&presets(&["rust", "python"])),
        Case::new("same/twice-in-one-list").project(&presets(&["docker", "nodejs", "docker"])),
        Case::new("empty/list").project(&presets(&[])),
        Case::new("empty/no-presets").project(&json!({})),
    ]
}

/// Layer bodies that overlap keys of the preset documents.
fn overlap_cases() -> Vec<Case> {
    vec![
        Case::new("overlap/plain-token-over-masked-local")
            .project(&presets(&["claude-code"]))
            .local(&json!({ "env": { "CLAUDE_CODE_OAUTH_TOKEN": "${OTHER_TOKEN}" } })),
        Case::new("overlap/plain-token-over-masked-same-file").project(&json!({
            "presets": ["claude-code"],
            "env": { "CLAUDE_CODE_OAUTH_TOKEN": "${OTHER_TOKEN}", "IS_SANDBOX": "0" },
        })),
        Case::new("overlap/plain-token-user-below-preset-project")
            .user(&json!({ "env": { "CLAUDE_CODE_OAUTH_TOKEN": "${USER_TOKEN}" } }))
            .project(&presets(&["claude-code"])),
        Case::new("overlap/claude-rule-disabled")
            .project(&presets(&["claude-code"]))
            .project_local(&json!({
                "network": { "rules": { "claude-code": { "enabled": false } } },
            })),
        Case::new("overlap/alpine-rule-disabled")
            .project(&presets(&["alpine", "python"]))
            .project_local(&json!({
                "network": { "rules": { "alpine-packages": { "enabled": false } } },
            })),
        Case::new("overlap/python-allow-concatenates").project(&json!({
            "presets": ["python"],
            "network": { "rules": { "python-packages": { "allow": ["pypi.internal.example.com"] } } },
        })),
        Case::new("overlap/python-allow-concatenates-across-layers")
            .user(&json!({
                "network": { "rules": { "python-packages": { "allow": ["user.example.com"] } } },
            }))
            .project(&json!({
                "presets": ["python"],
                "network": { "rules": { "python-packages": { "allow": ["project.example.com"] } } },
            })),
        Case::new("overlap/codex-dir-mount").project(&json!({
            "presets": ["openai-codex"],
            "mounts": { "codex-dir": { "source": "~/work/codex", "missing": "fail" } },
        })),
        Case::new("overlap/node-ca-shared")
            .project(&presets(&["claude-code", "nodejs"]))
            .project_local(&json!({ "env": { "NODE_EXTRA_CA_CERTS": "/custom/ca.pem" } })),
        Case::new("overlap/docker-daemon-timeout").project(&json!({
            "presets": ["docker"],
            "daemons": { "dockerd": { "timeout": 30 } },
        })),
        Case::new("overlap/codex-inject-extra-host").project(&json!({
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
fn released_presets_lists_resolve_as_golden_file() {
    let golden: Map<String, Value> = serde_json::from_str(GOLDEN).unwrap();
    let errors: Vec<_> = golden
        .iter()
        .filter(|(_, v)| v.get("error").is_some())
        .map(|(name, v)| format!("{name}: {}", v["error"]))
        .collect();
    assert!(errors.is_empty(), "{}", errors.join("\n"));

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

#[test]
#[ignore = "rewrites the golden file; run only for a deliberate preset document edit"]
fn regenerate_legacy_presets_golden() {
    std::fs::write(GOLDEN_PATH, render(&record_all())).unwrap();
}

#[test]
fn presets_list_names_are_plain_config_applied_once() {
    let dirs = ConfigDirs::new();
    dirs.user_file(".airlock.toml", "presets = [\"python\", \"python\"]\n")
        .project_file(
            "airlock.toml",
            "presets = [\"python\", \"claude-code\", \"copilot-cli\", \"openai-codex\"]\n",
        );
    let resolved = dirs.resolve().unwrap();
    assert!(resolved.packs.is_empty());
    for rule in ["claude-code", "copilot-cli", "codex"] {
        assert!(resolved.values.network.rules.contains_key(rule), "{rule}");
    }
    assert_eq!(
        resolved.values.network.rules["python-packages"].allow.len(),
        4
    );
}

#[test]
fn presets_list_and_pack_entry_of_same_pack_both_apply() {
    let dirs = ConfigDirs::new();
    dirs.user_file(".airlock.toml", "presets = [\"python\"]\n")
        .project_file("airlock.toml", "[packs]\npython = { version = 1 }\n");
    let resolved = dirs.resolve().unwrap();
    assert_eq!(resolved.packs.len(), 1);
    assert_eq!(
        resolved.values.network.rules["python-packages"].allow.len(),
        8
    );
}

#[test]
fn unknown_presets_list_names_are_errors_with_hints() {
    assert_eq!(
        project_toml_error("presets = [\"claude\"]\n"),
        "airlock.toml: unknown preset `claude` (known: claude-code, openai-codex, \
         copilot-cli, nodejs, python, rust, docker, alpine, arch, debian, fedora, suse); \
         newer packs use the [packs] table of a project config file, for example \
         `[packs] claude = { version = 1 }`"
    );
    assert!(project_toml_error("presets = [\"mise\"]\n").ends_with(
        "; newer packs use the [packs] table of a project config file, for example \
             `[packs] mise = { version = 1 }`"
    ));
    let err = project_toml_error("presets = [\"nonexistent\"]\n");
    assert!(
        err.starts_with("airlock.toml: unknown preset `nonexistent` (known: claude-code,"),
        "{err}"
    );
    assert!(err.ends_with("fedora, suse)"), "{err}");
}

#[test]
fn presets_value_that_is_not_list_of_names_fails_load() {
    for (yaml, text) in [
        (
            "presets: [1]\n",
            "`presets` list entries must be preset names (strings), not 1",
        ),
        (
            "presets: [python, 1]\n",
            "`presets` list entries must be preset names (strings), not 1",
        ),
        (
            "presets: true\n",
            "`presets` must be a list of preset names, not true",
        ),
        (
            "presets: python\n",
            "`presets` must be a list of preset names, not \"python\"",
        ),
        (
            "presets:\n  python: 1\n",
            "`presets` must be a list of preset names; use a [packs] table in a project \
             config file for versioned, installable packs",
        ),
    ] {
        let dirs = ConfigDirs::new();
        dirs.project_file("airlock.yaml", yaml);
        let err = format!("{:#}", dirs.load().err().unwrap());
        assert_eq!(
            err,
            format!("{}: {text}", dirs.project_origin("airlock.yaml")),
            "{yaml}"
        );
    }

    let dirs = ConfigDirs::new();
    dirs.project_file("airlock.yaml", "presets:\nvm:\n  cpus: 4\n");
    assert_eq!(dirs.values().vm.cpus, 4);
}
