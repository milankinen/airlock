//! Tests of the setup wizard form: keys, answers, the answer check and
//! the saved config.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use crate::cli::prompt::Step;
use crate::config::generated::Target;
use crate::config::{LayeredConfig, UserImage};
use crate::packs::{ArgValue, PackManager};
use crate::start::install::install_candidates;
use crate::start::wizard::form::{Form, Row, StartChoice};
use crate::start::wizard::{Input, check_answers, save_config};
use crate::test_cfg::{block_on, resolve_project_toml, temp_dir};
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
    &form.entries()[i].pack.metadata().name
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
    let vault = Vault::for_storage_type(VaultStorageType::Disabled);
    let input = Input {
        host_cwd: dir.path(),
        packs,
        config,
        vault: &vault,
    };
    block_on(check_answers(&input, &form.answers(Target::Local)))
}

/// Test that packs and args chosen with keys give a shared config that
/// resolves to the same choices.
///   1. Choose a distro, an agent and the sample pack, and give the sample
///      mode a value that its config refuses
///   2. Check that the error shows and the focus cannot leave the row,
///      then change the value
///   3. Disable the network arg, enable clipboard paste and choose start
///      and share
///   4. Check the saved `airlock.toml` text
///   5. Resolve the text and check the packs, args, image, network rule,
///      clipboard and installs
#[test]
fn choosing_packs_and_args_with_keys_saves_shared_config_that_resolves() {
    let packs = crate::packs::init_with_sample();
    let mut form = Form::new(&packs, None, StartChoice::Start);
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
    press(&mut form, KeyCode::Enter);
    assert_eq!(form.focus(), Row::Start);
    press(&mut form, KeyCode::Right);
    assert_eq!(form.start(), StartChoice::StartAndShare);
    let Step::Done(target) = press(&mut form, KeyCode::Enter) else {
        panic!("the wizard did not end with a start option");
    };
    assert_eq!(target, Target::Project);

    let config = LayeredConfig::from_values(vec![], None, vec![]).unwrap();
    check(&packs, &config, &form).unwrap();
    let dir = temp_dir();
    save_config(&form.answers(target).config(dir.path())).unwrap();
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
        .map(|c| c.metadata().name.as_str())
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
///   2. Start at once and check that the local config uses the user image
///      and has no packs
///   3. Open the form again, choose a distro pack and check that the
///      answers have no image and the distro pack
#[test]
fn user_image_is_preselected_until_distro_pack_is_chosen() {
    let packs = crate::packs::init_with_sample();
    let image = UserImage {
        name: "my/image:1".into(),
        value: serde_json::json!("my/image:1"),
    };
    let mut form = Form::new(&packs, Some(image.clone()), StartChoice::Start);
    assert_eq!(form.focus(), Row::Custom);
    press(&mut form, KeyCode::Enter);
    let Step::Done(target) = press(&mut form, KeyCode::Enter) else {
        panic!("the wizard did not end with a start option");
    };
    assert_eq!(target, Target::Local);
    let dir = temp_dir();
    let generated = form.answers(target).config(dir.path());
    assert_eq!(generated.path, dir.path().join(".airlock/airlock.toml"));
    let resolved = resolve_project_toml(&generated.toml).unwrap();
    assert!(resolved.packs.is_empty());
    assert_eq!(resolved.values.vm.image.name, "my/image:1");

    let mut form = Form::new(&packs, Some(image), StartChoice::Start);
    focus_pack(&mut form, "alpine");
    press(&mut form, KeyCode::Char(' '));
    let answers = form.answers(Target::Local);
    assert!(answers.image.is_none());
    assert_eq!(answers.packs[0].metadata().name, "alpine");
}

/// Test that the answer check fails when the env of the user config does
/// not resolve.
///   1. Check the answers with a plain env entry and check that they pass
///   2. Check them with an env entry that reads an unset host variable
///   3. Check that the error names the entry
#[test]
fn answers_whose_env_does_not_resolve_fail_check() {
    let packs = crate::packs::init_with_sample();
    let form = Form::new(&packs, None, StartChoice::Start);
    check(&packs, &user_files("[env]\nPLAIN = \"1\"\n"), &form).unwrap();
    let config = user_files("[env]\nMINE = \"${AIRLOCK_TEST_UNSET_VAR}\"\n");
    let e = check(&packs, &config, &form).unwrap_err();
    assert!(format!("{e:#}").contains("MINE"), "{e:#}");
}

/// Test that Esc, the cancel choice and Ctrl+C end the wizard with no
/// answers.
///   1. Go to the start row, press Esc and check that the focus goes back
///   2. Press Esc again and check that the wizard cancels
///   3. Choose cancel on the start row and check that Enter cancels
///   4. Press Ctrl+C and check that the wizard ends as interrupted
#[test]
fn esc_cancel_and_ctrl_c_end_wizard_without_answers() {
    let packs = crate::packs::init_with_sample();
    let mut form = Form::new(&packs, None, StartChoice::Start);
    let first = form.focus();
    press(&mut form, KeyCode::Enter);
    assert_eq!(form.focus(), Row::Start);
    assert!(matches!(press(&mut form, KeyCode::Esc), Step::Stay));
    assert_eq!(form.focus(), first);
    assert!(matches!(press(&mut form, KeyCode::Esc), Step::Cancel));

    let mut form = Form::new(&packs, None, StartChoice::Start);
    press(&mut form, KeyCode::Enter);
    press(&mut form, KeyCode::Right);
    press(&mut form, KeyCode::Right);
    press(&mut form, KeyCode::Right);
    assert_eq!(form.start(), StartChoice::Cancel);
    assert!(matches!(press(&mut form, KeyCode::Enter), Step::Cancel));

    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert!(matches!(form.key(ctrl_c), Step::Interrupt));
}

/// Test that the start option from the user settings is the first option
/// of the start bar, so that Enter on the start bar uses it.
///   1. Open the form with `start and share` first
///   2. Press Enter two times and check that the config is shareable
///   3. Open the form again, go left to `start` and check that the config
///      is local
#[test]
fn start_option_from_settings_is_first_choice_of_start_bar() {
    let packs = crate::packs::init_with_sample();
    let mut form = Form::new(&packs, None, StartChoice::StartAndShare);
    press(&mut form, KeyCode::Enter);
    assert!(matches!(
        press(&mut form, KeyCode::Enter),
        Step::Done(Target::Project)
    ));

    let mut form = Form::new(&packs, None, StartChoice::StartAndShare);
    press(&mut form, KeyCode::Enter);
    press(&mut form, KeyCode::Left);
    assert!(matches!(
        press(&mut form, KeyCode::Enter),
        Step::Done(Target::Local)
    ));
}
