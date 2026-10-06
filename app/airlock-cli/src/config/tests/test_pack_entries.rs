use std::collections::HashSet;

use crate::config::{ConfigOverrides, Layer, LayeredConfig, ResolvedConfig};
use crate::packs::{ArgValue, PackManager};
use crate::test_support::block_on;

const USER: &str = "~/.airlock/config.toml";
const LOCAL: &str = ".airlock/airlock.toml";
const PROJECT: &str = "airlock.toml";

/// The built-in packs and the test pack `sample@1` (args, including a
/// choice with `other`, and a `config.lua`).
fn packs() -> PackManager {
    crate::packs::init_with_sample()
}

/// Load layers from `(origin, toml)` files: [`USER`] is a user file,
/// [`LOCAL`] the local file, anything else a project file.
fn load(files: &[(&str, &str)]) -> anyhow::Result<LayeredConfig> {
    let mut user = vec![];
    let mut local = None;
    let mut project = vec![];
    for (origin, toml) in files {
        let layer = Layer::new(*origin, toml::from_str(toml).unwrap())?;
        match *origin {
            USER => user.push(layer),
            LOCAL => local = Some(layer),
            _ => project.push(layer),
        }
    }
    Ok(LayeredConfig::new(user, local, project))
}

/// [`load`] that must succeed.
fn files(files: &[(&str, &str)]) -> LayeredConfig {
    load(files).unwrap()
}

/// The config of one project file holding `toml`.
fn project(toml: &str) -> LayeredConfig {
    files(&[(PROJECT, toml)])
}

fn resolve(layers: &LayeredConfig) -> anyhow::Result<ResolvedConfig> {
    block_on(layers.resolve(&packs(), &ConfigOverrides::default()))
}

fn resolved(files_: &[(&str, &str)]) -> ResolvedConfig {
    resolve(&files(files_)).unwrap()
}

fn err_of(layers: &LayeredConfig) -> String {
    format!("{:#}", resolve(layers).err().expect("config error"))
}

fn err(toml: &str) -> String {
    err_of(&project(toml))
}

fn names(toml: &str) -> Vec<String> {
    resolve(&project(toml))
        .unwrap()
        .packs
        .iter()
        .map(|p| p.metadata().name.clone())
        .collect()
}

/// `(name, version)` of each configured pack.
fn versions(r: &ResolvedConfig) -> Vec<(&str, &str)> {
    r.packs
        .iter()
        .map(|p| (p.metadata().name.as_str(), p.metadata().version.as_str()))
        .collect()
}

fn arg<'a>(r: &'a ResolvedConfig, name: &str, key: &str) -> Option<&'a ArgValue> {
    r.packs
        .iter()
        .find(|p| p.metadata().name == name)?
        .args()
        .get(key)
}

fn text(s: &str) -> ArgValue {
    ArgValue::Text(s.into())
}

// -- entries and versions ----------------------------------------------

#[test]
fn enabled_packs_in_pack_order() {
    assert_eq!(
        names(
            "[packs]\npython = { version = 1 }\nclaude = { version = 1 }\n\
             docker = { version = 1, enabled = true }\nalpine = { version = 1 }\n"
        ),
        ["alpine", "claude", "docker", "python"]
    );
}

#[test]
fn no_packs_means_no_packs() {
    assert!(names("[vm]\ncpus = 1\n").is_empty());
}

#[test]
fn version_number_and_string_are_the_same() {
    let r = resolved(&[(
        PROJECT,
        "[packs]\npython = { version = 1 }\nrust = { version = \"1\" }\n",
    )]);
    assert_eq!(versions(&r), [("python", "1"), ("rust", "1")]);
}

#[test]
fn version_is_required_on_the_merged_entry() {
    let e = err("[packs]\nnodejs = {}\n");
    assert!(
        e.contains("`packs.nodejs` needs `version` (set in: airlock.toml): 1"),
        "{e}"
    );
    // Another file's entry gives the version.
    let r = resolved(&[
        (LOCAL, "[packs]\nnodejs = { version = 1 }\n"),
        (PROJECT, "[packs]\nnodejs = {}\n"),
    ]);
    assert_eq!(versions(&r), [("nodejs", "1")]);
    // Every contributing file is named.
    let e = err_of(&files(&[
        (LOCAL, "[packs]\nnodejs = {}\n"),
        ("airlock.local.toml", "[packs]\nnodejs = {}\n"),
    ]));
    assert!(
        e.contains("(set in: .airlock/airlock.toml, airlock.local.toml)"),
        "{e}"
    );
}

#[test]
fn invalid_versions_are_errors() {
    for (value, text) in [
        ("0", "`packs.python.version` must be 1 or higher"),
        ("-1", "`packs.python.version` must be 1 or higher"),
        (
            "1.5",
            "`packs.python.version` must be a string or a whole number, not 1.5",
        ),
        (
            "true",
            "`packs.python.version` must be a string or a whole number, not true",
        ),
    ] {
        let e = err(&format!("[packs]\npython = {{ version = {value} }}\n"));
        assert!(e.contains(text), "{value}: {e}");
        assert!(e.contains("(set in: airlock.toml)"), "{value}: {e}");
    }
}

#[test]
fn unsupported_version_is_an_error() {
    let e = err("[packs]\nmise = { version = \"legacy\" }\n");
    assert!(
        e.contains("`packs.mise`: version \"legacy\" is not supported (supported: \"1\")"),
        "{e}"
    );
    let e = err("[packs]\npython = { version = 2 }\n");
    assert!(
        e.contains("version \"2\" is not supported (supported: \"1\")"),
        "{e}"
    );
}

/// A released list name has no table version: the list form replaces it.
#[test]
fn version_legacy_is_an_error() {
    let e = err("[packs]\nclaude = { version = \"legacy\" }\n");
    assert!(
        e.contains(
            "* `packs.claude`: version \"legacy\" is not supported; use the list form \
             `presets = [\"claude-code\"]` (set in: airlock.toml)"
        ),
        "{e}"
    );
    let e = err("[packs]\ncodex = { version = \"legacy\" }\n");
    assert!(e.contains("`presets = [\"openai-codex\"]`"), "{e}");
}

#[test]
fn unknown_pack_and_list_name_are_errors() {
    let e = err("[packs]\nnope = { version = 1 }\n");
    assert!(e.contains("invalid configuration"), "{e}");
    assert!(
        e.contains("`packs.nope` unknown pack (known: alpine,"),
        "{e}"
    );
    let e = err("[packs]\nclaude-code = { version = 1 }\n");
    assert!(
        e.contains(
            "* `packs.claude-code` unknown pack (known: alpine, debian, claude, codex, copilot, \
             docker, git, mise, nodejs, python, rust, sample); `claude-code` is a list-form name: \
             write `presets = [\"claude-code\"]` (set in: airlock.toml)"
        ),
        "{e}"
    );
}

#[test]
fn pack_entry_must_be_a_table() {
    let e = err("[packs]\npython = true\n");
    assert!(e.contains("`packs.python` must be a table"), "{e}");
}

#[test]
fn enabled_must_be_bool() {
    let e = err("[packs]\npython = { version = 1, enabled = \"no\" }\n");
    assert!(
        e.contains("`packs.python.enabled` must be true or false"),
        "{e}"
    );
}

#[test]
fn all_problems_are_reported_together() {
    let e = err(
        "[packs]\nnope = {}\nsample = { version = 1, args = { mode = 1 } }\npython = 1\nrust = {}\n",
    );
    for text in [
        "packs.nope",
        "packs.sample.args.mode",
        "packs.python",
        "packs.rust",
    ] {
        assert!(e.contains(text), "{text}: {e}");
    }
}

// -- enabled ------------------------------------------------------------

#[test]
fn disabled_pack_adds_no_config() {
    let r = resolved(&[(
        PROJECT,
        "[packs]\npython = { version = 1, enabled = false }\n",
    )]);
    assert!(r.packs.is_empty());
    assert!(!r.values.network.rules.contains_key("python-packages"));
}

/// `enabled = false` needs no version and no valid args, in any file.
#[test]
fn enabled_false_needs_no_version_in_any_file() {
    assert!(names("[packs]\ncodex = { enabled = false }\n").is_empty());
    assert!(names("[packs]\nsample = { enabled = false, args = { mode = 1 } }\n").is_empty());
    // A list is not an entry: `enabled = false` does not turn it off.
    let r = resolved(&[
        (LOCAL, "[packs]\npython = { enabled = false }\n"),
        (PROJECT, "presets = [\"python\"]\n"),
    ]);
    assert!(r.packs.is_empty());
    assert!(r.values.network.rules.contains_key("python-packages"));
    let r = resolved(&[
        (LOCAL, "[packs]\npython = { enabled = false }\n"),
        (
            PROJECT,
            "[packs]\npython = { version = 1, enabled = true }\n",
        ),
    ]);
    assert_eq!(versions(&r), [("python", "1")]);
}

// -- args -------------------------------------------------------------------

#[test]
fn an_arg_is_parsed() {
    let r = resolved(&[(
        PROJECT,
        "[packs]\nsample = { version = 1, args = { mode = \"slow\" } }\n",
    )]);
    assert_eq!(arg(&r, "sample", "mode"), Some(&text("slow")));
    assert_eq!(arg(&r, "sample", "network"), Some(&ArgValue::Bool(true)));
    assert_eq!(r.packs[0].args().len(), 2);
}

/// A value that the pack does not support fails in its `config.lua`: a
/// config error with the hint of the pack.
#[test]
fn an_unsupported_value_is_an_error_with_a_hint() {
    let e = err("[packs]\nsample = { version = 1, args = { mode = \"broken\" } }\n");
    assert_eq!(
        e,
        "invalid configuration\n* pack sample: mode `broken` is not supported; use \
         `mode = \"fast\"`"
    );
}

#[test]
fn unknown_arg_lists_the_keys_of_the_version() {
    let e = err("[packs]\nsample = { version = 1, args = { mode = \"fast\", model = \"x\" } }\n");
    assert!(
        e.contains(
            "`packs.sample.args.model` unknown arg of version \"1\" (known: mode, network) \
             (set in: airlock.toml)"
        ),
        "{e}"
    );
    let e = err("[packs]\ndocker = { version = 1, args = { node-version = \"lts\" } }\n");
    assert!(
        e.contains(
            "`packs.docker.args.node-version` unknown arg of version \"1\" (it has no args)"
        ),
        "{e}"
    );
    let e = err("[packs]\nnodejs = { version = 1, node-version = \"lts\" }\n");
    assert!(
        e.contains(
            "`packs.nodejs.node-version` unknown key (known: version, enabled, args; args go \
             in `args = { node-version = … }`) (set in: airlock.toml)"
        ),
        "{e}"
    );
}

#[test]
fn a_wrong_arg_type_is_an_error() {
    let e = err("[packs]\nsample = { version = 1, args = { network = \"yes\" } }\n");
    assert!(
        e.contains("`packs.sample.args.network` must be true or false"),
        "{e}"
    );
    let e = err("[packs]\nsample = { version = 1, args = { mode = 1 } }\n");
    assert!(
        e.contains("`packs.sample.args.mode` must be a string"),
        "{e}"
    );
}

/// Each file's value is checked against the final version's type before
/// the merge.
#[test]
fn arg_values_are_checked_per_file() {
    let e = err_of(&files(&[
        (LOCAL, "[packs]\nsample = { version = 1 }\n"),
        (
            PROJECT,
            "[packs]\nsample = { args = { mode = [\"fast\"] } }\n",
        ),
    ]));
    assert!(
        e.contains(
            "`packs.sample.args.mode` must be a string (one of: fast, slow, or any other \
             non-empty string) (set in: airlock.toml)"
        ),
        "{e}"
    );
}

// -- the list form ------------------------------------------------------------

/// The list is taken out of the file's value when it loads; a table
/// stays for the pack entries.
#[test]
fn the_list_is_taken_out_at_load() {
    let layer = Layer::new(PROJECT, toml::from_str("presets = [\"python\"]\n").unwrap()).unwrap();
    assert_eq!(layer.legacy_presets, Some(vec!["python".to_string()]));
    assert!(layer.value.get("presets").is_none());
    let layer = Layer::new(
        PROJECT,
        toml::from_str("[packs]\npython = { version = 1 }\n").unwrap(),
    )
    .unwrap();
    assert!(layer.legacy_presets.is_none());
    assert!(layer.value["packs"]["python"].is_object());
}

/// List names are plain config: no pack entries, the released
/// documents apply.
#[test]
fn list_names_are_plain_config() {
    let r = resolved(&[(
        PROJECT,
        "presets = [\"claude-code\", \"copilot-cli\", \"openai-codex\"]\n",
    )]);
    assert!(r.packs.is_empty());
    for rule in ["claude-code", "copilot-cli", "codex"] {
        assert!(r.values.network.rules.contains_key(rule), "{rule}");
    }
}

/// A name applies once, in one list or across files.
#[test]
fn a_list_name_applies_once() {
    let once = resolved(&[(PROJECT, "presets = [\"python\"]\n")]);
    let allow = &once.values.network.rules["python-packages"].allow;
    assert_eq!(allow.len(), 4);
    let r = resolved(&[
        (USER, "presets = [\"python\", \"python\"]\n"),
        (PROJECT, "presets = [\"python\"]\n"),
    ]);
    assert_eq!(&r.values.network.rules["python-packages"].allow, allow);
}

#[test]
fn short_and_unknown_names_in_a_list_are_errors() {
    let e = err("presets = [\"claude\"]\n");
    assert_eq!(
        e,
        "airlock.toml: unknown preset `claude` (known: claude-code, openai-codex, \
         copilot-cli, nodejs, python, rust, docker, alpine, arch, debian, fedora, suse); newer packs use the [packs] table of a project config file, for \
         example `[packs] claude = { version = 1 }`"
    );
    let e = err("presets = [\"mise\"]\n");
    assert!(
        e.ends_with(
            "; newer packs use the [packs] table of a project config file, for example \
             `[packs] mise = { version = 1 }`"
        ),
        "{e}"
    );
    let e = err("presets = [\"nonexistent\"]\n");
    assert!(
        e.starts_with("airlock.toml: unknown preset `nonexistent` (known: claude-code,"),
        "{e}"
    );
    assert!(e.ends_with("fedora, suse)"), "{e}");
}

#[test]
fn presets_value_types_are_checked() {
    for (value, text) in [
        (
            serde_json::json!({ "presets": [1] }),
            "airlock.yaml: `presets` list entries",
        ),
        (
            serde_json::json!({ "presets": true }),
            "airlock.yaml: `presets` must be a list",
        ),
    ] {
        let e = format!(
            "{:#}",
            Layer::new("airlock.yaml", value).err().expect("load error")
        );
        assert!(e.starts_with(text), "{e}");
    }
}

/// A `presets` of `null` (a YAML `presets:` with no value, or an explicit
/// JSON `null`) is treated as if the key were absent, not an error.
#[test]
fn presets_null_is_treated_as_absent() {
    let json = serde_json::json!({ "presets": null, "cpus": 4 });
    let yaml: serde_json::Value = serde_yaml::from_str("presets:\ncpus: 4\n").unwrap();
    for value in [json, yaml] {
        let layer = Layer::new("airlock.yaml", value).expect("load");
        assert_eq!(layer.legacy_presets, None);
        assert_eq!(layer.value["cpus"], 4);
    }
}

// -- merge across files ---------------------------------------------------------

/// The examples of the merge rules: `version` from the highest file that
/// sets it; args only from files with that version or none.
#[test]
fn entries_merge_across_files() {
    let r = resolved(&[
        (USER, "presets = [\"openai-codex\"]\n"),
        (PROJECT, "[packs]\ncodex = { version = 1 }\n"),
    ]);
    assert_eq!(versions(&r), [("codex", "1")]);

    let r = resolved(&[
        (
            LOCAL,
            "[packs]\nsample = { version = 1, args = { mode = \"slow\" } }\n",
        ),
        (PROJECT, "[packs]\nsample = { version = 1 }\n"),
    ]);
    assert_eq!(arg(&r, "sample", "mode"), Some(&text("slow")));
}

/// A list name and a table entry of the same pack both apply: the
/// documents of both, arrays concatenated.
#[test]
fn a_pack_in_a_list_and_a_table_both_apply() {
    let r = resolved(&[
        (USER, "presets = [\"python\"]\n"),
        (PROJECT, "[packs]\npython = { version = 1 }\n"),
    ]);
    assert_eq!(versions(&r), [("python", "1")]);
    assert_eq!(r.values.network.rules["python-packages"].allow.len(), 8);

    let r = resolved(&[
        (LOCAL, "[packs]\ncodex = { version = 1 }\n"),
        (PROJECT, "presets = [\"openai-codex\"]\n"),
    ]);
    assert_eq!(versions(&r), [("codex", "1")]);
    assert!(r.values.env.contains_key("OPENAI_API_KEY"));

    // A version-less table entry does not take its version from a list.
    let e = err_of(&files(&[
        (LOCAL, "[packs]\ncodex = {}\n"),
        (PROJECT, "presets = [\"openai-codex\"]\n"),
    ]));
    assert!(
        e.contains("`packs.codex` needs `version` (set in: .airlock/airlock.toml): 1"),
        "{e}"
    );
}

// -- documents -------------------------------------------------------------------

#[test]
fn pack_documents_apply() {
    let config = resolve(&project(
        "[packs]\npython = { version = 1 }\nrust = { version = 1 }\n",
    ))
    .unwrap()
    .values;
    assert!(config.network.rules.contains_key("python-packages"));
    assert!(config.network.rules.contains_key("rust-packages"));
    for key in ["SSL_CERT_FILE", "REQUESTS_CA_BUNDLE", "PIP_CERT"] {
        let var = &config.env[key];
        assert_eq!(var.value, "/etc/ssl/certs/ca-certificates.crt", "{key}");
        assert!(!var.mask, "{key}");
    }
}

#[test]
fn files_override_pack_documents() {
    let config = resolve(&project(
        "[packs]\npython = { version = 1 }\n[env]\nSSL_CERT_FILE = \"/x.pem\"\n",
    ))
    .unwrap()
    .values;
    assert_eq!(config.env["SSL_CERT_FILE"].value, "/x.pem");
}

// -- the built-in packs ------------------------------------------------------------------

#[test]
fn builtin_packs_are_valid() {
    let mut seen = HashSet::new();
    for pack in packs().builtin() {
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
