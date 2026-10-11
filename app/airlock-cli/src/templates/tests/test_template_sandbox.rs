//! Tests of the template flow: build a template, then start a new sandbox
//! from it.

use crate::config::LayeredConfig;
use crate::config::generated::{GeneratedConfig, NewEntry, Target};
use crate::packs::install::state::PackStatus;
use crate::templates::{self, FILES};
use crate::test_cfg::packs::test_packs;
use crate::test_cfg::start::{StartProject, fake_host};
use crate::vm::disk::DISK_FILE;

/// Take the packs whose install script ran since the last call.
fn ran() -> Vec<String> {
    fake_host(|host| std::mem::take(&mut host.ran))
}

/// The config files of a wizard answer: no user files, and the generated
/// project config `toml`, made from the template `template` if given.
fn wizard_config(project: &StartProject, toml: &str, template: Option<&str>) -> LayeredConfig {
    let generated = GeneratedConfig {
        target: Target::Local,
        path: Target::Local.path(&project.project_dir()),
        toml: toml.to_string(),
        template: template.map(str::to_string),
    };
    LayeredConfig::from_values(vec![], None, vec![])
        .unwrap()
        .with_generated_project(generated)
        .unwrap()
}

/// Test that a template keeps its disk, install records and a config with
/// a pinned image, and that a new sandbox from it installs only the packs
/// that the template does not have.
///   1. Build a template with the pack alpha
///   2. Check the template files, and that the config pins the image digest
///   3. Start a new sandbox from the template with the extra pack beta
///   4. Check that only beta installs, that the records of the new disk
///      have both packs, and that the sandbox has the template CA
///   5. Start again and check that nothing installs and no question stops
///      the start
#[test]
fn sandbox_from_template_installs_only_extra_packs() {
    let project = StartProject::new(test_packs());
    let data_dir = &project.context().data_dir;
    let base = "[packs]\nalpha = { version = 1 }\n";
    project
        .build_template("base", &wizard_config(&project, base, None), false)
        .unwrap();
    assert_eq!(ran(), ["alpha"]);

    let template = templates::find(data_dir, "base").unwrap();
    for file in FILES.iter().filter(|f| **f != templates::IMAGE_FILE) {
        assert!(template.dir.join(file).is_file(), "{file}");
    }
    let image = template.config.image.clone().expect("a pinned image");
    assert!(image.contains("@sha256:"), "{image}");
    assert_eq!(templates::list(data_dir).len(), 1, "no build directory");

    let beta = NewEntry {
        name: "beta",
        version: "1",
        args: vec![],
    };
    let generated = GeneratedConfig::new(
        &project.project_dir(),
        Target::Local,
        &[beta],
        None,
        None,
        Some(&template),
    );
    let toml = generated.toml.clone();
    let config = LayeredConfig::from_values(vec![], None, vec![])
        .unwrap()
        .with_generated_project(generated)
        .unwrap();
    project.start_config(&config, false, false).unwrap();
    assert_eq!(ran(), ["beta"]);
    let state = project.state();
    assert_eq!(state.disk, project.disk_id());
    assert_ne!(
        project.disk_id(),
        crate::vm::disk::read_id(&template.dir),
        "the copy has its own disk identity"
    );
    for pack in ["alpha", "beta"] {
        assert_eq!(state.packs[pack].status, PackStatus::Installed, "{pack}");
    }
    assert!(project.sandbox_dir().join(DISK_FILE).is_file());
    let ca = |dir: &std::path::Path| std::fs::read(dir.join(templates::CA_FILE)).unwrap();
    assert_eq!(ca(&project.sandbox_dir()), ca(&template.dir), "same CA");

    project.accept_config(&toml);
    project.start(&toml, false).unwrap();
    assert!(ran().is_empty());
}

/// Test that a template name must be valid, that an existing name needs
/// a replace, and that a removed template is gone from the list.
///   1. Check valid and not valid names
///   2. Build a template, and check that a second build with the same name
///      fails and keeps the template
///   3. Build again with replace and check that the new template replaces
///      the old one, with no other directories in the list
///   4. Remove the template and check that the list is empty
#[test]
fn existing_template_needs_replace_and_removed_template_is_gone() {
    for name in ["base", "rust-1.80", "a_b.c"] {
        assert!(templates::is_valid_name(name), "{name}");
    }
    for name in ["", ".build-x", "-x", "a/b", "a b", &"x".repeat(65)] {
        assert!(!templates::is_valid_name(name), "{name}");
    }
    let project = StartProject::new(test_packs());
    let data_dir = &project.context().data_dir;
    let alpha = "[packs]\nalpha = { version = 1 }\n";
    let beta = "[packs]\nbeta = { version = 1 }\n";
    project
        .build_template("base", &wizard_config(&project, alpha, None), false)
        .unwrap();
    let packs = |name: &str| -> Vec<String> {
        let template = templates::find(data_dir, name).unwrap();
        template.config.packs.into_iter().map(|p| p.name).collect()
    };
    assert!(
        project
            .build_template("base", &wizard_config(&project, beta, None), false)
            .is_err()
    );
    assert_eq!(packs("base"), ["alpha"]);

    project
        .build_template("base", &wizard_config(&project, beta, None), true)
        .unwrap();
    assert_eq!(packs("base"), ["beta"]);
    let entries: Vec<_> = std::fs::read_dir(templates::templates_dir(data_dir))
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .collect();
    assert_eq!(entries, ["base"]);

    templates::remove(data_dir, "base").unwrap();
    assert!(templates::list(data_dir).is_empty());
    assert!(templates::remove(data_dir, "base").is_err());
}
