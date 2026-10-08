//! Tests for the config that the setup wizard writes: the text it makes,
//! the in-memory project layer, and the save to the project or local file.

use crate::config::config_values::PullPolicy;
use crate::config::generated::{Clipboard, GeneratedConfig, NewEntry, Target};
use crate::config::{ConfigOverrides, ResolvedConfig};
use crate::packs::ArgValue;
use crate::test_cfg::{ConfigDirs, resolve_layers};

/// Return a wizard pack entry at version 1 with the given args.
fn entry<'a>(name: &'a str, args: Vec<(&'a str, &'a ArgValue)>) -> NewEntry<'a> {
    NewEntry {
        name,
        version: "1",
        args,
    }
}

/// Check that the config has the wizard choices of the first test: the
/// claude and sample packs, sample in slow mode, the user image, and copy
/// without paste.
fn assert_wizard_choices(resolved: &ResolvedConfig) {
    let installers: Vec<String> = resolved
        .packs
        .iter()
        .filter_map(crate::packs::ConfiguredPack::setup_installer)
        .map(|i| i.pack)
        .collect();
    assert_eq!(installers, ["claude", "sample"]);
    assert_eq!(
        resolved.packs[1].non_default_args(),
        [("mode", &ArgValue::Text("slow".into()))]
    );
    let config = &resolved.values;
    assert!(config.network.rules.contains_key("sample"));
    assert_eq!(config.vm.image.name, "user:1");
    assert_eq!(config.vm.image.pull_policy, PullPolicy::IfChanged);
    assert!(config.clipboard.copy);
    assert!(!config.clipboard.paste);
}

/// Test that the wizard config gives the same result from memory and from
/// disk. The start command uses the config before the save, so both must
/// agree. A second save must not overwrite a file that appeared.
///   1. Make the project config with two packs, the user image and clipboard
///      choices, and check its TOML text
///   2. Add it as an in-memory project layer and check the resolved choices
///   3. Save it, load the files again and check the same choices
///   4. Save a different config and check that it fails and the file stays
#[test]
fn wizard_project_config_resolves_before_save_and_after_reload() {
    let dirs = ConfigDirs::new();
    dirs.user_file(
        ".airlock.toml",
        "[vm.image]\nname = \"user:1\"\npull-policy = \"if-changed\"\n",
    );
    let layers = dirs.load().unwrap();
    assert!(!layers.has_project_config());
    let image = layers.user_image().unwrap();
    assert_eq!(image.name, "user:1");

    let slow = ArgValue::Text("slow".into());
    let generated = GeneratedConfig::new(
        &dirs.project(),
        Target::Project,
        &[
            entry("claude", vec![]),
            entry("sample", vec![("mode", &slow)]),
        ],
        Some(&image.value),
        Some(Clipboard {
            copy: true,
            paste: false,
        }),
    );
    assert_eq!(generated.path, dirs.project().join("airlock.toml"));
    assert_eq!(
        generated.toml,
        "[packs]\n\
         claude = { version = \"1\" }\n\
         sample = { version = \"1\", args = { mode = \"slow\" } }\n\
         [vm.image]\n\
         name = \"user:1\"\n\
         pull-policy = \"if-changed\"\n\
         \n\
         [clipboard]\n\
         copy = true\n\
         paste = false\n"
    );

    let layers = layers.with_generated_project(generated.clone()).unwrap();
    assert!(layers.has_project_config());
    assert!(layers.generated_project().is_some());
    assert_wizard_choices(&resolve_layers(&layers, ConfigOverrides::default()).unwrap());
    // A second in-memory project layer is an error.
    assert!(layers.with_generated_project(generated.clone()).is_err());

    generated.save().unwrap();
    assert_eq!(
        std::fs::read_to_string(&generated.path).unwrap(),
        generated.toml
    );
    assert!(!dirs.project().join(".airlock").exists());
    assert_wizard_choices(&dirs.resolve().unwrap());

    // The project file exists now, so the save of a new config must fail.
    let other = GeneratedConfig::new(&dirs.project(), Target::Project, &[], None, None);
    let err = other.save().unwrap_err().to_string();
    assert!(err.contains("appeared"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&generated.path).unwrap(),
        generated.toml
    );
}

/// Test that a local wizard config goes to `.airlock/` with a `.gitignore`,
/// so that git does not see the local file.
///   1. Save an empty config for the local target
///   2. Check the `.gitignore`, and that no `airlock.toml` exists
///   3. Check that the load finds a project config
///   4. Check that a second save fails
#[test]
fn wizard_local_config_is_saved_after_gitignore() {
    let dirs = ConfigDirs::new();
    let generated = GeneratedConfig::new(&dirs.project(), Target::Local, &[], None, None);
    assert_eq!(generated.toml, "");
    assert_eq!(generated.path, dirs.project().join(".airlock/airlock.toml"));
    generated.save().unwrap();
    assert_eq!(
        std::fs::read_to_string(dirs.project().join(".airlock/.gitignore")).unwrap(),
        "*\n"
    );
    assert!(!dirs.project().join("airlock.toml").exists());
    assert!(dirs.load().unwrap().has_project_config());
    assert!(generated.save().is_err());
}
