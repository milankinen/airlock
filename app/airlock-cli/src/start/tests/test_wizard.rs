//! Tests of the setup wizard form: keys, answers, the answer check and
//! the saved config.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cli::prompt::Step;
use crate::config::generated::Target;
use crate::config::{LayeredConfig, UserImage};
use crate::packs::{ArgValue, PackKind, PackManager};
use crate::start::install::install_candidates;
use crate::start::wizard::form::{Form, Mode, Row, StartChoice};
use crate::start::wizard::{Input, check_answers, save_config, view};
use crate::templates::Template;
use crate::test_cfg::oci::{data_dir_at, verified_dir};
use crate::test_cfg::packs::load_packs;
use crate::test_cfg::{block_on, resolve_project_toml, temp_dir, test_template};
use crate::vault::{Vault, VaultStorageType};

/// Press `code` without modifiers.
fn press(form: &mut Form, code: KeyCode) -> Step<Target> {
    form.key(KeyEvent::new(code, KeyModifiers::NONE))
}

/// Type `text`, one key for each character.
fn type_text(form: &mut Form, text: &str) {
    for c in text.chars() {
        press(form, KeyCode::Char(c));
    }
}

/// Press Down until `found` is true. Panics after 100 presses.
fn press_down_until(form: &mut Form, found: impl Fn(&Form) -> bool) {
    for _ in 0..100 {
        if found(form) {
            return;
        }
        press(form, KeyCode::Down);
    }
    panic!("row not found");
}

/// The name of the pack of form entry `i`.
fn pack_name(form: &Form, i: usize) -> &str {
    form.entries()[i].pack.metadata().id.as_str()
}

/// Move the focus down to the row of pack `name`.
fn focus_pack(form: &mut Form, name: &str) {
    press_down_until(
        form,
        |form| matches!(form.focus(), Row::Pack(i) if pack_name(form, i) == name),
    );
}

/// Move the focus down to the row of arg `key` of pack `name`.
fn focus_arg(form: &mut Form, name: &str, key: &str) {
    press_down_until(form, |form| {
        matches!(form.focus(), Row::Arg(i, a)
            if pack_name(form, i) == name && form.entries()[i].pack.args()[a].key == key)
    });
}

/// Return true if pack `name` is selected.
fn is_selected(form: &Form, name: &str) -> bool {
    form.entries()
        .iter()
        .any(|e| e.selected && e.pack.metadata().id == name)
}

/// The value of arg `key` of pack `name`.
fn arg_value<'a>(form: &'a Form, name: &str, key: &str) -> &'a ArgValue {
    let entry = form
        .entries()
        .iter()
        .find(|e| e.pack.metadata().id == name)
        .unwrap();
    &entry.values[key]
}

/// The row of pack `name`.
fn pack_row(form: &Form, name: &str) -> Row {
    let i = (0..form.entries().len())
        .find(|i| pack_name(form, *i) == name)
        .unwrap();
    Row::Pack(i)
}

/// The template `base`: debian with `allow-apt` off, sample in slow mode, a
/// pack that is not known, a pinned image and clipboard paste.
fn base_template() -> Template {
    test_template(
        "base",
        "[packs]\n\
         debian = { version = \"1\", args = { allow-apt = false } }\n\
         sample = { version = \"1\", args = { mode = \"slow\", network = \"bad\" } }\n\
         ghost = { version = \"1\" }\n\n\
         [vm]\nimage = \"debian@sha256:0123\"\n\n\
         [clipboard]\ncopy = true\npaste = true\n",
    )
}

/// The text of the view of `form`, without colors.
fn view_text(form: &Form) -> String {
    let frame = view::frame(form, 200);
    console::strip_ansi_codes(&frame.lines.join("\n")).to_string()
}

/// A layered config with only the user file `user_toml`.
fn user_files(user_toml: &str) -> LayeredConfig {
    LayeredConfig::from_values(
        vec![("user", toml::from_str(user_toml).unwrap())],
        None,
        vec![],
    )
    .unwrap()
}

/// Check the local answers of `form` against `config`, as the wizard does
/// before it ends.
fn check(packs: &PackManager, config: &LayeredConfig, form: &Form) -> anyhow::Result<()> {
    let dir = temp_dir();
    let data = temp_dir();
    let vault = Vault::for_storage_type(VaultStorageType::Disabled);
    let (host_cwd, data_dir) = (verified_dir(dir.path()), data_dir_at(data.path()));
    let input = Input {
        host_cwd: &host_cwd,
        data_dir: &data_dir,
        packs,
        config,
        vault: &vault,
        templates: &[],
        mode: Mode::Start,
    };
    block_on(check_answers(&input, &form.answers(Target::Local)))
}

/// Test that packs and args chosen with keys give a shared config that
/// resolves to the same choices.
///   1. Choose a distro, an agent and the sample pack, and give the sample
///      mode a value that its config refuses
///   2. Check that the error shows and the focus cannot leave the row,
///      then change the value
///   3. Disable the network arg, enable clipboard paste and
///      `Create airlock.toml`, and choose start
///   4. Check the saved `airlock.toml` text
///   5. Resolve the text and check the packs, args, image, network rule,
///      clipboard and installs
#[test]
fn choosing_packs_and_args_with_keys_saves_shared_config_that_resolves() {
    let packs = crate::packs::init_with_sample();
    let data = temp_dir();
    let mut form = Form::new(&data_dir_at(data.path()), &packs, None, &[], Mode::Start);
    focus_pack(&mut form, "debian");
    press(&mut form, KeyCode::Char(' '));
    focus_pack(&mut form, "claude");
    press(&mut form, KeyCode::Char(' '));
    focus_pack(&mut form, "sample");
    press(&mut form, KeyCode::Char(' '));
    focus_arg(&mut form, "sample", "mode");
    // Two steps to the right go past `fast` and `slow` to the free value.
    press(&mut form, KeyCode::Right);
    press(&mut form, KeyCode::Right);
    assert!(form.other().is_some());
    type_text(&mut form, "broken");
    let mode_row = form.focus();
    press(&mut form, KeyCode::Down);
    assert_eq!(form.focus(), mode_row);
    let error = form.other().unwrap().error.clone().unwrap();
    assert!(error.contains("mode `broken` is not supported"), "{error}");
    for _ in 0.."broken".len() {
        press(&mut form, KeyCode::Backspace);
    }
    type_text(&mut form, "turbo");
    press(&mut form, KeyCode::Down);
    focus_arg(&mut form, "sample", "network");
    press(&mut form, KeyCode::Char(' '));
    press_down_until(&mut form, |form| form.focus() == Row::ClipboardPaste);
    press(&mut form, KeyCode::Char(' '));
    press(&mut form, KeyCode::Down);
    assert_eq!(form.focus(), Row::ShareConfig);
    press(&mut form, KeyCode::Char(' '));
    press(&mut form, KeyCode::Enter);
    assert_eq!(form.focus(), Row::Start);
    assert_eq!(form.start(), StartChoice::Start);
    let Step::Done(target) = press(&mut form, KeyCode::Enter) else {
        panic!("the wizard did not end with a start option");
    };
    assert_eq!(target, Target::Project);

    let config = LayeredConfig::from_values(vec![], None, vec![]).unwrap();
    check(&packs, &config, &form).unwrap();
    let dir = temp_dir();
    let project = verified_dir(dir.path());
    save_config(
        &form.answers(target).config(&project),
        &project.join(".airlock").unwrap(),
    )
    .unwrap();
    let text = std::fs::read_to_string(dir.path().join("airlock.toml")).unwrap();
    assert_eq!(
        text,
        "[packs]\ndebian = { version = \"1\", args = { allow-apt = true } }\n\
         claude = { version = \"1\", args = { acp = false } }\n\
         sample = { version = \"1\", args = { mode = \"turbo\", network = false } }\n\n\
         [clipboard]\ncopy = true\npaste = true\n"
    );

    let resolved = resolve_project_toml(&text).unwrap();
    let names: Vec<&str> = resolved
        .packs
        .iter()
        .map(|c| c.metadata().id.as_str())
        .collect();
    assert_eq!(names, ["debian", "claude", "sample"]);
    assert_eq!(
        resolved.packs[2].args()["mode"],
        ArgValue::Text("turbo".into())
    );
    assert!(resolved.values.vm.image.name.starts_with("debian"));
    assert!(!resolved.values.network.rules.contains_key("sample"));
    assert!(resolved.values.clipboard.paste);
    let installs: Vec<String> = install_candidates(&resolved.packs)
        .into_iter()
        .map(|i| i.pack)
        .collect();
    // The distro pack sets the image and has no install.
    assert_eq!(installs, ["claude", "sample"]);
}

/// Test that the image of the user config is chosen first, and that a
/// distro pack replaces it.
///   1. Open the form with a user image and check that the custom image
///      row has the focus
///   2. Start at once and check that the config is local, uses the user
///      image and has no packs
///   3. Open the form again, choose a distro pack and check that the
///      answers have no image and the distro pack
#[test]
fn user_image_is_preselected_until_distro_pack_is_chosen() {
    let packs = crate::packs::init_with_sample();
    let data = temp_dir();
    let image = UserImage {
        name: "my/image:1".into(),
        value: serde_json::json!("my/image:1"),
    };
    let mut form = Form::new(
        &data_dir_at(data.path()),
        &packs,
        Some(image.clone()),
        &[],
        Mode::Start,
    );
    assert_eq!(form.focus(), Row::Custom);
    press(&mut form, KeyCode::Enter);
    let Step::Done(target) = press(&mut form, KeyCode::Enter) else {
        panic!("the wizard did not end with a start option");
    };
    assert_eq!(target, Target::Local);
    let dir = temp_dir();
    let generated = form.answers(target).config(&verified_dir(dir.path()));
    assert_eq!(
        generated.path(),
        dir.path()
            .join(".airlock/airlock.toml")
            .display()
            .to_string()
    );
    let resolved = resolve_project_toml(&generated.toml).unwrap();
    assert!(resolved.packs.is_empty());
    assert_eq!(resolved.values.vm.image.name, "my/image:1");

    let mut form = Form::new(
        &data_dir_at(data.path()),
        &packs,
        Some(image),
        &[],
        Mode::Start,
    );
    focus_pack(&mut form, "alpine");
    press(&mut form, KeyCode::Char(' '));
    let answers = form.answers(Target::Local);
    assert!(answers.image.is_none());
    assert_eq!(answers.packs[0].metadata().id, "alpine");
}

/// Test that the answer check fails when the env of the user config does
/// not resolve.
///   1. Check the answers with a plain env entry and check that they pass
///   2. Check them with an env entry that reads an unset host variable
///   3. Check that the error names the entry
#[test]
fn answers_whose_env_does_not_resolve_fail_check() {
    let packs = crate::packs::init_with_sample();
    let data = temp_dir();
    let form = Form::new(&data_dir_at(data.path()), &packs, None, &[], Mode::Start);
    check(&packs, &user_files("[env]\nPLAIN = \"1\"\n"), &form).unwrap();
    let config = user_files("[env]\nMINE = \"${AIRLOCK_TEST_UNSET_VAR}\"\n");
    let e = check(&packs, &config, &form).unwrap_err();
    assert!(format!("{e:#}").contains("MINE"), "{e:#}");
}

/// Test that Esc, the abort choice and Ctrl+C end the wizard with no
/// answers.
///   1. Go to the start row, press Esc and check that the focus goes back
///   2. Press Esc again and check that the wizard aborts
///   3. Choose abort on the start row and check that Enter aborts
///   4. Press Ctrl+C and check that the wizard ends as interrupted
#[test]
fn esc_abort_and_ctrl_c_end_wizard_without_answers() {
    let packs = crate::packs::init_with_sample();
    let data = temp_dir();
    let mut form = Form::new(&data_dir_at(data.path()), &packs, None, &[], Mode::Start);
    let first = form.focus();
    press(&mut form, KeyCode::Enter);
    assert_eq!(form.focus(), Row::Start);
    assert!(matches!(press(&mut form, KeyCode::Esc), Step::Stay));
    assert_eq!(form.focus(), first);
    assert!(matches!(press(&mut form, KeyCode::Esc), Step::Abort));

    let mut form = Form::new(&data_dir_at(data.path()), &packs, None, &[], Mode::Start);
    press(&mut form, KeyCode::Enter);
    press(&mut form, KeyCode::Right);
    assert_eq!(form.start(), StartChoice::Abort);
    assert!(matches!(press(&mut form, KeyCode::Enter), Step::Abort));

    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(form.key(ctrl_c), Step::Interrupt));
}

/// Test that the template section is first and only shows when templates
/// exist. The empty template must be the default, so a new user gets the
/// old wizard.
///   1. Open the form without templates and check that there are no
///      template rows and the focus is on the first distro pack
///   2. Open it with two templates and check the rows: empty template
///      first, then the templates in order
///   3. Check that the focus is on the empty template, and that it is
///      selected
#[test]
fn template_section_is_first_only_when_templates_exist() {
    let packs = crate::packs::init_with_sample();
    let data = temp_dir();
    let form = Form::new(&data_dir_at(data.path()), &packs, None, &[], Mode::Start);
    assert!(form.template_rows().is_empty());
    assert_eq!(form.focus(), pack_row(&form, "alpine"));
    assert!(!view_text(&form).contains("Template"));

    let templates = [base_template(), test_template("other", "")];
    let form = Form::new(
        &data_dir_at(data.path()),
        &packs,
        None,
        &templates,
        Mode::Start,
    );
    assert_eq!(
        form.template_rows(),
        [
            Row::Template(None),
            Row::Template(Some(0)),
            Row::Template(Some(1))
        ]
    );
    assert_eq!(form.focus(), Row::Template(None));
    assert!(form.selected_template().is_none());
    let text = view_text(&form);
    assert!(text.contains("empty - No preinstalled packs"), "{text}");
}

/// Test that a pack with no kind shows in the "Other" section, after the
/// tools. The section is only there when such a pack exists.
///   1. Open the form with the built-in packs and the sample pack, and
///      check that there is no
///      "Other" section
///   2. Open the form with a tool pack and a pack with no kind
///   3. Check that the pack with no kind is the only row of the "Other"
///      section, and that the section comes after the "Tools" section
#[test]
fn pack_without_kind_shows_in_other_section_after_tools() {
    let data = temp_dir();
    let lines = |form: &Form| -> Vec<String> {
        view_text(form)
            .lines()
            .map(str::trim)
            .map(String::from)
            .collect()
    };
    let form = Form::new(
        &data_dir_at(data.path()),
        &crate::packs::init_with_sample(),
        None,
        &[],
        Mode::Start,
    );
    assert!(!lines(&form).iter().any(|l| l == "Other"));

    let packs = load_packs(vec![
        (
            "hammer@1/pack.toml",
            "label = \"Hammer\"\ndescription = \"A tool\"\nkind = \"tool\"\n".into(),
        ),
        (
            "misc@1/pack.toml",
            "label = \"Misc\"\ndescription = \"No kind\"\n".into(),
        ),
    ]);
    let form = Form::new(&data_dir_at(data.path()), &packs, None, &[], Mode::Start);
    assert_eq!(
        form.section_rows(PackKind::Other),
        [pack_row(&form, "misc")]
    );
    let lines = lines(&form);
    let at = |text: &str| lines.iter().position(|l| l.contains(text)).unwrap();
    let title = |text: &str| lines.iter().position(|l| l == text).unwrap();
    let order = [title("Tools"), at("Hammer"), title("Other"), at("Misc")];
    assert!(order.is_sorted(), "{lines:?}");
}

/// Test that a template selects and locks its packs and the distro group,
/// and that the empty template unlocks them again. The template config
/// already has these packs, so the answers must not repeat them.
///   1. Select the template and check the packs, args, clipboard and locks
///   2. Move the focus through all rows and check that it skips the locked
///      rows
///   3. Select an extra agent and check the answers and the config
///   4. Select the empty template and check that the template packs are
///      off with default args, the first distro is back and the agent
///      stays
#[test]
fn template_locks_its_packs_until_empty_template_is_selected() {
    let packs = crate::packs::init_with_sample();
    let data = temp_dir();
    let templates = [base_template()];
    let mut form = Form::new(
        &data_dir_at(data.path()),
        &packs,
        None,
        &templates,
        Mode::Start,
    );
    press(&mut form, KeyCode::Down);
    assert_eq!(form.focus(), Row::Template(Some(0)));
    assert!(view_text(&form).contains("base - packs: debian, sample"));
    press(&mut form, KeyCode::Char(' '));
    assert_eq!(form.selected_template().unwrap().name, "base");
    assert!(is_selected(&form, "debian"));
    assert!(!is_selected(&form, "alpine"));
    assert!(is_selected(&form, "sample"));
    assert_eq!(
        arg_value(&form, "debian", "allow-apt"),
        &ArgValue::Bool(false)
    );
    assert_eq!(
        arg_value(&form, "sample", "mode"),
        &ArgValue::Text("slow".into())
    );
    // The template value has the wrong type, thus the default stays.
    assert_eq!(arg_value(&form, "sample", "network"), &ArgValue::Bool(true));
    assert!(form.clipboard().paste);
    for name in ["alpine", "debian", "sample"] {
        assert!(form.is_locked(pack_row(&form, name)), "{name}");
    }
    assert!(!form.is_locked(pack_row(&form, "claude")));

    // One round through all rows comes back to the template row.
    let mut seen = Vec::new();
    loop {
        press(&mut form, KeyCode::Down);
        if form.focus() == Row::Template(Some(0)) {
            break;
        }
        seen.push(form.focus());
    }
    assert!(seen.iter().all(|row| !form.is_locked(*row)), "{seen:?}");
    assert!(seen.contains(&Row::ShareConfig));
    assert_eq!(seen[0], pack_row(&form, "claude"));

    focus_pack(&mut form, "claude");
    press(&mut form, KeyCode::Char(' '));
    let answers = form.answers(Target::Local);
    assert_eq!(answers.template.as_ref().unwrap().name, "base");
    assert!(answers.image.is_none());
    let names: Vec<&str> = answers
        .packs
        .iter()
        .map(|c| c.metadata().id.as_str())
        .collect();
    assert_eq!(names, ["claude"]);
    let generated = answers.config(&verified_dir(data.path()));
    assert_eq!(generated.template.as_deref(), Some("base"));
    assert!(
        generated.toml.contains(
            "ghost = { version = \"1\" }\nclaude = { version = \"1\", args = { acp = false } }\n"
        ),
        "{}",
        generated.toml
    );

    press_down_until(&mut form, |form| form.focus() == Row::Template(None));
    press(&mut form, KeyCode::Char(' '));
    assert!(form.selected_template().is_none());
    assert!(is_selected(&form, "alpine"));
    assert!(!is_selected(&form, "debian"));
    assert!(!is_selected(&form, "sample"));
    assert!(is_selected(&form, "claude"));
    assert_eq!(
        arg_value(&form, "debian", "allow-apt"),
        &ArgValue::Bool(true)
    );
    assert_eq!(
        arg_value(&form, "sample", "mode"),
        &ArgValue::Text("fast".into())
    );
    assert!(!form.is_locked(pack_row(&form, "debian")));
    let answers = form.answers(Target::Local);
    assert!(answers.template.is_none());
    assert_eq!(answers.packs.len(), 2);
}

/// Test that a template change restores the user choices that the earlier
/// template replaced. Otherwise a template could leave, for example,
/// clipboard paste on without the user seeing it.
///   1. Select debian and the sample pack with network off
///   2. Select the template that has them and sets clipboard paste
///   3. Change to a template without packs and clipboard, and check that
///      the sample pack, its args and the clipboard come back
///   4. Select the empty template and check that debian comes back too
#[test]
fn template_change_restores_user_packs_and_clipboard() {
    let packs = crate::packs::init_with_sample();
    let data = temp_dir();
    let templates = [base_template(), test_template("other", "")];
    let mut form = Form::new(
        &data_dir_at(data.path()),
        &packs,
        None,
        &templates,
        Mode::Start,
    );
    focus_pack(&mut form, "debian");
    press(&mut form, KeyCode::Char(' '));
    focus_pack(&mut form, "sample");
    press(&mut form, KeyCode::Char(' '));
    focus_arg(&mut form, "sample", "network");
    press(&mut form, KeyCode::Char(' '));
    assert_eq!(
        arg_value(&form, "sample", "network"),
        &ArgValue::Bool(false)
    );

    press_down_until(&mut form, |form| form.focus() == Row::Template(Some(0)));
    press(&mut form, KeyCode::Char(' '));
    assert_eq!(arg_value(&form, "sample", "network"), &ArgValue::Bool(true));
    assert!(form.clipboard().paste);

    press(&mut form, KeyCode::Down);
    assert_eq!(form.focus(), Row::Template(Some(1)));
    press(&mut form, KeyCode::Char(' '));
    assert!(is_selected(&form, "sample"));
    assert!(!form.is_locked(pack_row(&form, "sample")));
    assert_eq!(
        arg_value(&form, "sample", "network"),
        &ArgValue::Bool(false)
    );
    assert_eq!(
        arg_value(&form, "sample", "mode"),
        &ArgValue::Text("fast".into())
    );
    assert!(!form.clipboard().paste);
    // The template locks the distro group and has no distro pack.
    assert!(!form.distro_selected());

    press_down_until(&mut form, |form| form.focus() == Row::Template(None));
    press(&mut form, KeyCode::Char(' '));
    assert!(is_selected(&form, "debian"));
    assert!(!is_selected(&form, "alpine"));
    assert!(is_selected(&form, "sample"));
    assert!(!form.clipboard().paste);
}

/// Test that the template mode has no shared config option and that the
/// start bar creates the template. A template is not a project file, so
/// the option makes no sense there.
///   1. Open the form in template mode and move through all rows
///   2. Check that no row is the shared config row
///   3. Check that the view shows `create` and no "Config" section
///   4. Press Enter twice and check that the wizard ends with a local
///      target
#[test]
fn template_mode_has_no_share_row_and_creates() {
    let packs = crate::packs::init_with_sample();
    let data = temp_dir();
    let mut form = Form::new(&data_dir_at(data.path()), &packs, None, &[], Mode::Template);
    let first = form.focus();
    loop {
        assert_ne!(form.focus(), Row::ShareConfig);
        press(&mut form, KeyCode::Down);
        if form.focus() == first {
            break;
        }
    }
    let text = view_text(&form);
    assert!(text.contains("enter create"), "{text}");
    assert!(text.contains("« create »"), "{text}");
    assert!(!text.contains("Config"), "{text}");
    press(&mut form, KeyCode::Enter);
    assert!(matches!(
        press(&mut form, KeyCode::Enter),
        Step::Done(Target::Local)
    ));
}

/// Test that the wizard offers user packs with the `(local)` marker and
/// selects the packs of the `packs.defaults` setting. The saved config
/// must name the user pack with a quoted key and no version.
///   1. Load a user pack `notes` and the defaults `claude`, `~notes` and
///      the unknown `~ghost`
///   2. Check that `~ghost` is reported as unknown
///   3. Open the form and check the marker and the selected packs
///   4. Start with `Create airlock.toml` and check the saved text
///   5. Resolve the saved text and parse it as a template config
#[test]
fn user_pack_shows_local_marker_and_settings_defaults_are_preselected() {
    let tmp = temp_dir();
    let dir = tmp.path().join("packs");
    crate::test_cfg::packs::write_user_pack(
        &dir,
        "notes",
        &[(
            "pack.toml",
            "label = \"Notes\"\ndescription = \"Keeps notes\"\nkind = \"tool\"\n",
        )],
    );
    let (packs, _) =
        crate::test_cfg::packs::with_user_packs(&[&dir], &["claude", "~notes", "~ghost"]).unwrap();
    assert_eq!(packs.defaults().1, ["~ghost"]);

    let data = temp_dir();
    let mut form = Form::new(&data_dir_at(data.path()), &packs, None, &[], Mode::Start);
    let text = view_text(&form);
    assert!(text.lines().any(|l| l.contains("Notes (local)")), "{text}");
    assert!(is_selected(&form, "claude"));
    assert!(is_selected(&form, "~notes"));
    assert!(!is_selected(&form, "sample"));

    press_down_until(&mut form, |form| form.focus() == Row::ShareConfig);
    press(&mut form, KeyCode::Char(' '));
    press(&mut form, KeyCode::Enter);
    let Step::Done(target) = press(&mut form, KeyCode::Enter) else {
        panic!("the wizard did not end with a start option");
    };
    let project_dir = temp_dir();
    let project = verified_dir(project_dir.path());
    save_config(
        &form.answers(target).config(&project),
        &project.join(".airlock").unwrap(),
    )
    .unwrap();
    let text = std::fs::read_to_string(project_dir.path().join("airlock.toml")).unwrap();
    assert!(text.contains("\n\"~notes\" = {}\n"), "{text}");

    let resolved = crate::test_cfg::packs::resolve_with(&packs, &text).unwrap();
    assert!(resolved.packs.iter().any(|p| p.metadata().id == "~notes"));
    let template = crate::templates::TemplateConfig::parse(&text).unwrap();
    let notes = template.packs.iter().find(|p| p.name == "~notes").unwrap();
    assert_eq!(notes.version, None);
}

/// Test that the selection of a user pack in the wizard is its approval.
/// The user saw the pack in the wizard, so `airlock start` does not ask
/// again. The answer check runs the `config.lua` of the pack, so the
/// approval must come first.
///   1. Load a user pack with a `config.lua` and select it with the
///      `packs.defaults` setting
///   2. Check the answers without the approval: check the approval error
///   3. Record the approval of the selected packs and check the answers
///      again: check that they pass
#[test]
fn wizard_selection_approves_user_pack_before_answer_check() {
    let tmp = temp_dir();
    let dir = tmp.path().join("packs");
    crate::test_cfg::packs::write_user_pack(
        &dir,
        "notes",
        &[
            (
                "pack.toml",
                "label = \"Notes\"\ndescription = \"Keeps notes\"\nkind = \"tool\"\n",
            ),
            ("config.lua", "config.env = { NOTES = \"on\" }\n"),
        ],
    );
    let (packs, _) = crate::test_cfg::packs::with_user_packs(&[&dir], &["~notes"]).unwrap();
    let dir = temp_dir();
    let data = temp_dir();
    let vault = Vault::for_storage_type(VaultStorageType::Disabled);
    let (host_cwd, data_dir) = (verified_dir(dir.path()), data_dir_at(data.path()));
    let config = user_files("");
    let input = Input {
        host_cwd: &host_cwd,
        data_dir: &data_dir,
        packs: &packs,
        config: &config,
        vault: &vault,
        templates: &[],
        mode: Mode::Start,
    };
    let form = Form::new(&data_dir, &packs, None, &[], Mode::Start);
    assert!(is_selected(&form, "~notes"));
    let answers = form.answers(Target::Local);

    let error = block_on(check_answers(&input, &answers)).unwrap_err();
    assert!(
        error.is::<crate::packs::approval::PacksNeedApproval>(),
        "{error:#}"
    );

    form.approve_selected().unwrap();
    block_on(check_answers(&input, &answers)).unwrap();
}
