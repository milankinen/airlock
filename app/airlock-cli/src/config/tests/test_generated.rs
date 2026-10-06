use crate::config::generated::{GeneratedConfig, NewEntry, Target};
use crate::packs::ArgValue;
use crate::test_support::{TempDir, resolve_project_toml};

fn mode(value: &str) -> ArgValue {
    ArgValue::Text(value.into())
}

/// The entry of the pack `name` at version "1" with `args`.
fn entry<'a>(name: &'a str, args: Vec<(&'a str, &'a ArgValue)>) -> NewEntry<'a> {
    NewEntry {
        name,
        version: "1",
        args,
    }
}

/// The content of a new project file.
fn render(entries: &[NewEntry<'_>]) -> String {
    GeneratedConfig::new(
        std::path::Path::new("/p"),
        Target::Project,
        entries,
        None,
        None,
    )
    .toml
}

#[test]
fn render_writes_the_chosen_packs_only() {
    let slow = mode("slow");
    let text = render(&[
        entry("sample", vec![("mode", &slow)]),
        entry("python", vec![]),
    ]);
    assert_eq!(
        text,
        "[packs]\nsample = { version = \"1\", args = { mode = \"slow\" } }\npython = { version = \"1\" }\n"
    );
    assert_eq!(render(&[]), "");
}

/// A file that the wizard renders resolves with the chosen packs as
/// installs.
#[test]
fn a_rendered_file_loads_with_its_install_candidates() {
    let slow = mode("slow");
    let text = render(&[
        entry("claude", vec![]),
        entry("sample", vec![("mode", &slow)]),
    ]);
    let resolved = resolve_project_toml(&text).unwrap();
    let names: Vec<String> = resolved
        .packs
        .iter()
        .filter_map(crate::packs::ConfiguredPack::setup_installer)
        .map(|i| i.pack)
        .collect();
    assert_eq!(names, ["claude", "sample"]);
    assert_eq!(resolved.packs[1].non_default_args(), [("mode", &slow)]);
    assert!(resolved.values.network.rules.contains_key("sample"));
    assert_eq!(resolved.values.vm.image.name, "alpine:latest");
}

#[test]
fn save_writes_a_new_file_in_the_chosen_place() {
    let tmp = TempDir::new("wizard-save");
    let generated = GeneratedConfig::new(
        tmp.path(),
        Target::Project,
        &[entry("python", vec![])],
        None,
        None,
    );
    assert_eq!(generated.path, tmp.path().join("airlock.toml"));
    generated.save().unwrap();
    let text = "[packs]\npython = { version = \"1\" }\n";
    assert_eq!(std::fs::read_to_string(&generated.path).unwrap(), text);
    // Only the chosen file is written.
    assert!(!tmp.path().join(".airlock/airlock.toml").exists());

    // An existing file is an error and stays as it was.
    let other = GeneratedConfig::new(tmp.path(), Target::Project, &[], None, None);
    let e = other.save().unwrap_err();
    assert!(e.to_string().contains("appeared"), "{e}");
    assert_eq!(std::fs::read_to_string(&generated.path).unwrap(), text);
}

#[test]
fn save_local_writes_the_gitignore_first() {
    let tmp = TempDir::new("wizard-save-local");
    let generated = GeneratedConfig::new(tmp.path(), Target::Local, &[], None, None);
    assert_eq!(generated.path, tmp.path().join(".airlock/airlock.toml"));
    generated.save().unwrap();
    assert_eq!(
        std::fs::read_to_string(tmp.path().join(".airlock/.gitignore")).unwrap(),
        "*\n"
    );
    assert!(!tmp.path().join("airlock.toml").exists());
    assert!(generated.save().is_err());
}
