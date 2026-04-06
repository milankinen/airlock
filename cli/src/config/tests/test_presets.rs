use std::collections::HashSet;

use include_dir::{Dir, include_dir};

use crate::config::load_config::{apply_with_presets, extract_presets, parse_config};
use crate::config::presets;

static FIXTURES: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/src/config/tests/fixtures");

fn fixture_resolve(name: &str) -> Option<serde_json::Value> {
    let file = FIXTURES
        .files()
        .find(|f| f.path().file_stem().is_some_and(|s| s == name))?;
    let toml_str = std::str::from_utf8(file.contents()).ok()?;
    Some(toml::from_str(toml_str).unwrap())
}

fn json(toml_str: &str) -> serde_json::Value {
    toml::from_str(toml_str).unwrap()
}

fn empty() -> serde_json::Value {
    serde_json::Value::Object(serde_json::Map::new())
}

fn resolve(config: serde_json::Value) -> serde_json::Value {
    apply_with_presets(
        empty(),
        config,
        &fixture_resolve,
        &mut vec![],
        &mut HashSet::new(),
    )
    .unwrap()
}

fn resolve_err(config: serde_json::Value) -> String {
    apply_with_presets(
        empty(),
        config,
        &fixture_resolve,
        &mut vec![],
        &mut HashSet::new(),
    )
    .unwrap_err()
    .to_string()
}

// -- extract_presets -------------------------------------------------

#[test]
fn extract_returns_names_and_strips_key() {
    let mut val = json(r#"presets = ["a", "b"]"#);
    let names = extract_presets(&mut val);
    assert_eq!(names, vec!["a", "b"]);
    assert!(val.as_object().unwrap().get("presets").is_none());
}

#[test]
fn extract_returns_empty_when_missing() {
    let mut val = json(r"cpus = 4");
    assert!(extract_presets(&mut val).is_empty());
}

// -- apply_with_presets ----------------------------------------------

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

#[test]
fn nested_presets_resolved_recursively() {
    // test-nested has presets=["test-base"] and cpus=4
    // test-base has image="test:base", cpus=2
    // expected: test-base applied first, then test-nested overrides cpus
    let result = resolve(json(
        r#"
        presets = ["test-nested"]
    "#,
    ));
    assert_eq!(result["image"], "test:base"); // from test-base (via test-nested)
    assert_eq!(result["cpus"], 4); // test-nested overrides test-base's 2
}

#[test]
fn circular_preset_detected() {
    let err = resolve_err(json(r#"presets = ["test-cycle-a"]"#));
    assert!(err.contains("circular preset dependency"));
}

#[test]
fn diamond_dependency_applied_once() {
    // Both test-diamond-left and test-diamond-right depend on test-diamond-base.
    // test-diamond-base: cpus=2, network.allowed_hosts=["example.com"]
    // test-diamond-left: memory="1 GB"
    // test-diamond-right: cpus=4
    let result = resolve(json(
        r#"
        presets = ["test-diamond-left", "test-diamond-right"]
    "#,
    ));
    // diamond-base applied once, then left, then right
    assert_eq!(result["cpus"], 4); // right overrides base's 2
    assert_eq!(result["memory"], "1 GB"); // from left
    let hosts = result["network"]["allowed_hosts"].as_array().unwrap();
    assert_eq!(hosts.len(), 1); // applied once, not duplicated
}

#[test]
fn unknown_preset_errors() {
    let err = resolve_err(json(r#"presets = ["nonexistent"]"#));
    assert!(err.contains("unknown preset"));
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
    let config = json(
        r#"
        presets = ["test-base"]
    "#,
    );
    let merged = apply_with_presets(
        empty(),
        config,
        &fixture_resolve,
        &mut vec![],
        &mut HashSet::new(),
    )
    .unwrap();
    parse_config(merged).unwrap();
}

// -- bundled presets --------------------------------------------------

#[test]
fn all_bundled_presets_are_valid() {
    for (name, value) in presets::all() {
        let merged = apply_with_presets(
            empty(),
            value,
            &presets::get,
            &mut vec![],
            &mut HashSet::new(),
        )
        .unwrap_or_else(|e| panic!("preset `{name}` fails to resolve: {e}"));
        parse_config(merged).unwrap_or_else(|e| panic!("preset `{name}` fails to parse: {e}"));
    }
}
