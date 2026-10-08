use crate::config::config_values::PullPolicy;
use crate::config::generated::{Clipboard, GeneratedConfig, NewEntry, Target};
use crate::config::{ConfigOverrides, ResolvedConfig};
use crate::packs::ArgValue;
use crate::test_cfg::{ConfigDirs, resolve_layers};

fn entry<'a>(name: &'a str, args: Vec<(&'a str, &'a ArgValue)>) -> NewEntry<'a> {
    NewEntry {
        name,
        version: "1",
        args,
    }
}

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
    assert!(layers.with_generated_project(generated.clone()).is_err());

    generated.save().unwrap();
    assert_eq!(
        std::fs::read_to_string(&generated.path).unwrap(),
        generated.toml
    );
    assert!(!dirs.project().join(".airlock").exists());
    assert_wizard_choices(&dirs.resolve().unwrap());

    let other = GeneratedConfig::new(&dirs.project(), Target::Project, &[], None, None);
    let err = other.save().unwrap_err().to_string();
    assert!(err.contains("appeared"), "{err}");
    assert_eq!(
        std::fs::read_to_string(&generated.path).unwrap(),
        generated.toml
    );
}

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
