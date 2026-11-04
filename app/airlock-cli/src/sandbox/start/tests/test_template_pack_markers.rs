//! Tests of the template pack markers of the setup wizard: a user pack of
//! a template changed or is gone since the template install.

use crate::config::LayeredConfig;
use crate::config::generated::{GeneratedConfig, Target};
use crate::sandbox::start::wizard::form::{Form, Mode, Row};
use crate::test_cfg::oci::verified_dir;
use crate::test_cfg::packs::{with_user_packs, write_user_pack};
use crate::test_cfg::start::{StartProject, view_text};
use crate::test_cfg::temp_dir;

/// The `pack.toml` of a user pack with the label `label`.
fn pack_toml(label: &str) -> String {
    format!("label = \"{label}\"\ndescription = \"A tool\"\nkind = \"tool\"\n")
}

/// Test that the wizard tells about template user packs that changed or
/// are gone since the template install. The user must know before the
/// start that the sandbox asks about the changed pack, and that the
/// config fails without the missing pack.
///   1. Build a template with the user packs `~edit`, `~keep` and `~gone`
///   2. Change the setup script of `~edit`, and remove `~gone`
///   3. Open the wizard and check that the template row shows
///      `! missing: ~gone`
///   4. Select the template and check that only `~edit` shows `! changed`
#[test]
fn template_user_pack_that_changed_or_is_gone_shows_marker() {
    let packs_dir = temp_dir();
    for (name, label) in [("edit", "Edit"), ("keep", "Keep"), ("gone", "Gone")] {
        let toml = pack_toml(label);
        write_user_pack(
            packs_dir.path(),
            name,
            &[("pack.toml", &toml), ("setup.sh", "echo one\n")],
        );
    }
    let (packs, _) = with_user_packs(&[packs_dir.path()], &[]).unwrap();
    let project = StartProject::new(packs);
    let generated = GeneratedConfig {
        target: Target::Local,
        project: verified_dir(&project.project_dir()),
        toml: "[packs]\n\"~edit\" = {}\n\"~keep\" = {}\n\"~gone\" = {}\n".to_string(),
        template: None,
    };
    let config = LayeredConfig::from_values(vec![], None, vec![])
        .unwrap()
        .with_generated_project(generated)
        .unwrap();
    project.build_template("base", &config, false).unwrap();

    std::fs::write(packs_dir.path().join("edit/setup.sh"), "echo two\n").unwrap();
    std::fs::remove_dir_all(packs_dir.path().join("gone")).unwrap();
    let (packs, _) = with_user_packs(&[packs_dir.path()], &[]).unwrap();
    let data = &project.context().data;
    let templates = crate::templates::list(data);
    let mut form = Form::new(data, &packs, None, &templates, Mode::Start);
    let shown = view_text(&form);
    assert!(shown.contains("base ! missing: ~gone"), "{shown}");
    assert!(!shown.contains("! changed"), "{shown}");

    form.key(crossterm::event::KeyCode::Down.into());
    assert_eq!(form.focus(), Row::Template(Some(0)));
    form.key(crossterm::event::KeyCode::Char(' ').into());
    let shown = view_text(&form);
    assert!(shown.contains("Edit (local) ! changed"), "{shown}");
    assert!(shown.contains("Keep (local)\n"), "{shown}");
}
