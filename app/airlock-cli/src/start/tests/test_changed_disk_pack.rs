//! Tests of the changed-pack question of `airlock start`: a pack on the
//! disk has a changed definition. The question shows the changes since the
//! install, and the answers re-create the sandbox, reinstall the pack on
//! top, or keep the old pack.

use std::path::Path;

use crate::config::LayeredConfig;
use crate::config::generated::{GeneratedConfig, NewEntry, Target};
use crate::packs::ArgValue;
use crate::packs::install::snapshots::{self, SNAPSHOTS_FILE};
use crate::packs::install::state::PackStatus;
use crate::start::Exit;
use crate::start::sandbox::ChangedAnswer;
use crate::test_cfg::packs::{test_installers, test_packs, with_user_packs, write_user_pack};
use crate::test_cfg::start::{StartProject, fake_host};
use crate::test_cfg::temp_dir;
use crate::util::PinnedDir;

/// The config of the first start: two packs with a setup script.
const BASE: &str = "[packs]\nalpha = { version = 1 }\nbeta = { version = 1 }\n";
/// [`BASE`] with another arg value for `alpha`.
const NEW_ARGS: &str =
    "[packs]\nalpha = { version = 1, args = { mode = \"slow\" } }\nbeta = { version = 1 }\n";
/// A value that the question must never show.
const SECRET: &str = "s3cret-token";
/// The config of the user pack `~tool`.
const TOOL: &str = "[packs]\n\"~tool\" = {}\n";

/// Take the packs whose install script ran since the last call.
fn ran() -> Vec<String> {
    fake_host(|host| std::mem::take(&mut host.ran))
}

/// Set the answer to the changed-packs question. `None`: no terminal.
fn answer(answer: Option<ChangedAnswer>) {
    fake_host(|host| host.changed_answer = answer);
}

/// Take the lines of the changed-packs questions since the last call.
fn questions() -> Vec<Vec<String>> {
    fake_host(|host| std::mem::take(&mut host.changed_questions))
}

/// The exit code of a start. Panics on an unexpected error.
fn code(result: Result<(), Exit>) -> i32 {
    match result {
        Ok(()) => 0,
        Err(Exit::Code(code)) => code,
        Err(Exit::Failed(e)) => panic!("{e:#}"),
    }
}

/// The install fingerprint of `alpha` in the config `toml`.
fn alpha_fingerprint(toml: &str) -> String {
    test_installers(toml)
        .into_iter()
        .find(|t| t.pack == "alpha")
        .unwrap()
        .fingerprint
}

/// The install snapshots of the sandbox of `project`.
fn sandbox_snapshots(project: &StartProject) -> snapshots::InstallSnapshots {
    snapshots::read(&PinnedDir::pin(&project.sandbox_dir()).unwrap())
}

/// Test that each answer to the changed-pack question of an existing
/// sandbox does what it tells, and that the question shows the arg change.
///   1. Start with alpha and beta, then change an arg of alpha
///   2. Answer "Re-create" and check that the disk is new and both packs
///      install again
///   3. Answer "Reinstall on top" and check that only alpha installs on the
///      same disk, and that the install snapshot has the new arg
///   4. Answer "Keep old" and check that nothing installs, the record has
///      the new fingerprint and keeps its status, and that the next start
///      without a terminal asks nothing
///   5. Check that each question shows the arg diff of alpha
#[test]
fn changed_pack_answers_re_create_reinstall_or_keep_old_pack() {
    for choice in [
        ChangedAnswer::Recreate,
        ChangedAnswer::Reinstall,
        ChangedAnswer::KeepOld,
    ] {
        let project = StartProject::new(test_packs());
        project.start(BASE, false).unwrap();
        ran();
        let disk = project.disk_id();
        let old = alpha_fingerprint(BASE);
        assert!(
            sandbox_snapshots(&project).get(&old).is_some(),
            "{choice:?}"
        );
        project.accept_config(NEW_ARGS);

        answer(Some(choice));
        project.start(NEW_ARGS, false).unwrap();
        answer(None);
        let new = alpha_fingerprint(NEW_ARGS);
        let state = project.state();
        match choice {
            ChangedAnswer::Recreate => {
                assert_ne!(project.disk_id(), disk);
                assert_eq!(ran(), ["alpha", "beta"]);
            }
            ChangedAnswer::Reinstall => {
                assert_eq!(project.disk_id(), disk);
                assert_eq!(ran(), ["alpha"]);
                let snapshot = sandbox_snapshots(&project).get(&new).cloned().unwrap();
                assert_eq!(snapshot.args["mode"], "slow");
                assert!(sandbox_snapshots(&project).get(&old).is_none());
            }
            ChangedAnswer::KeepOld => {
                assert_eq!(project.disk_id(), disk);
                assert!(ran().is_empty());
                // The copy of the old install stays for the next diff.
                let snapshot = sandbox_snapshots(&project).get(&new).cloned().unwrap();
                assert_eq!(snapshot.args["mode"], "fast");
            }
        }
        assert_eq!(state.packs["alpha"].fingerprint, new, "{choice:?}");
        assert_eq!(
            state.packs["alpha"].status,
            PackStatus::Installed,
            "{choice:?}"
        );
        let lines = questions().concat();
        assert!(
            lines.contains(&"Changes of pack alpha (Alpha) since the install:".to_string()),
            "{lines:?}"
        );
        for line in ["version and args", "- mode = fast", "+ mode = slow"] {
            assert!(lines.contains(&line.to_string()), "{line}: {lines:?}");
        }

        project.start(NEW_ARGS, false).unwrap();
        assert!(ran().is_empty(), "{choice:?}");
        assert!(questions().is_empty(), "{choice:?}");
    }
}

/// Write the user pack `~tool` into `dir`, with the setup script `setup`.
/// Its `config.lua` sets the setup value `TOKEN` from the file `token`.
fn write_tool(dir: &Path, setup: &str, token: &Path) {
    let lua = format!(
        "local file = assert(io.open(\"{}\"))\nsetup.env.TOKEN = file:read(\"a\")\nfile:close()\n",
        token.display()
    );
    write_user_pack(
        dir,
        "tool",
        &[
            ("pack.toml", "label = \"Tool\"\ndescription = \"A tool\"\n"),
            ("config.lua", &lua),
            ("setup.sh", setup),
        ],
    );
}

/// Test that the question of a changed user pack shows the file diff since
/// the install and the names of the changed setup values, but never the
/// values. The user must see what installs on top of the disk.
///   1. Start with the user pack `~tool`
///   2. Change its setup script and its setup value
///   3. Answer "Reinstall on top" and check that the pack installs again on
///      the same disk
///   4. Check that the question shows the diff of `setup.sh`, the changed
///      setup value name, no unchanged file and no setup value
#[test]
fn changed_user_pack_question_shows_file_diff_and_setup_value_names() {
    let packs_dir = temp_dir();
    let token = packs_dir.path().join("token");
    std::fs::write(&token, "first").unwrap();
    write_tool(packs_dir.path(), "echo one\n", &token);
    let (packs, _) = with_user_packs(&[packs_dir.path()], &[]).unwrap();
    let mut project = StartProject::new(packs);
    project.start(TOOL, false).unwrap();
    assert_eq!(ran(), ["~tool"]);
    let disk = project.disk_id();

    write_tool(packs_dir.path(), "echo two\n", &token);
    std::fs::write(&token, SECRET).unwrap();
    let (packs, _) = with_user_packs(&[packs_dir.path()], &[]).unwrap();
    project.set_packs(packs);
    // Without a terminal, nobody can answer.
    assert_eq!(code(project.start(TOOL, false)), 2);
    answer(Some(ChangedAnswer::Reinstall));
    project.start(TOOL, false).unwrap();
    answer(None);
    assert_eq!(ran(), ["~tool"]);
    assert_eq!(project.disk_id(), disk);

    let lines = questions().concat();
    for line in [
        "setup.sh",
        "- echo one",
        "+ echo two",
        "Setup values changed: TOKEN",
    ] {
        assert!(lines.contains(&line.to_string()), "{line}: {lines:?}");
    }
    let text = lines.join("\n");
    assert!(!text.contains("config.lua"), "{text}");
    assert!(!text.contains(SECRET), "{text}");
    assert!(!text.contains("first"), "{text}");
}

/// The config files of a sandbox from the template `template` with the
/// pack `alpha` with the arg `mode`.
fn from_template(project: &StartProject, template: &str, mode: &str) -> LayeredConfig {
    let template = crate::templates::find(&project.context().data, template).unwrap();
    let mode = ArgValue::Text(mode.to_string());
    let alpha = NewEntry {
        name: "alpha",
        version: Some("1"),
        args: vec![("mode", &mode)],
    };
    let generated = GeneratedConfig::new(
        &crate::test_cfg::oci::verified_dir(&project.project_dir()),
        Target::Local,
        &[alpha],
        None,
        None,
        Some(&template),
    );
    LayeredConfig::from_values(vec![], None, vec![])
        .unwrap()
        .with_generated_project(generated)
        .unwrap()
}

/// The config files of a wizard answer that makes a template from `toml`,
/// with the base template `base` if given.
fn template_config(project: &StartProject, toml: &str, base: Option<&str>) -> LayeredConfig {
    let generated = GeneratedConfig {
        target: Target::Local,
        project: crate::test_cfg::oci::verified_dir(&project.project_dir()),
        toml: toml.to_string(),
        template: base.map(str::to_string),
    };
    LayeredConfig::from_values(vec![], None, vec![])
        .unwrap()
        .with_generated_project(generated)
        .unwrap()
}

/// A project with no sandbox, and the template `base` with alpha. The
/// fake host counters are clear.
fn with_base_template() -> StartProject {
    let project = StartProject::new(test_packs());
    let base = "[packs]\nalpha = { version = 1 }\n";
    project
        .build_template("base", &template_config(&project, base, None), false)
        .unwrap();
    assert_eq!(ran(), ["alpha"]);
    project
}

/// Test that a new sandbox from a template asks about a pack that changed
/// since the template install. Before the fix, the old pack stayed with no
/// question.
///   1. Build a template with alpha, and check that it has the install
///      snapshots
///   2. Start a new sandbox from it with another arg of alpha and no
///      terminal: check exit code 2, and that the template copy is gone
///   3. For each answer, start a new sandbox from the template and check
///      what installs: "Re-create" and "Reinstall on top" install alpha,
///      "Keep old" installs nothing
#[test]
fn sandbox_from_template_asks_about_changed_pack() {
    let project = with_base_template();
    let template = crate::templates::find(&project.context().data, "base").unwrap();
    assert!(
        template
            .dir
            .to_path()
            .unwrap()
            .join(SNAPSHOTS_FILE)
            .is_file()
    );
    let config = from_template(&project, "base", "slow");
    assert_eq!(code(project.start_config(&config, false, false)), 2);
    assert!(
        !project
            .sandbox()
            .is_some_and(|found| crate::project::has_disk(&found.dir)),
        "the template copy is gone"
    );
    assert!(ran().is_empty());

    for choice in [
        ChangedAnswer::Recreate,
        ChangedAnswer::Reinstall,
        ChangedAnswer::KeepOld,
    ] {
        let project = with_base_template();
        let config = from_template(&project, "base", "slow");
        answer(Some(choice));
        project.start_config(&config, false, false).unwrap();
        answer(None);
        let installed: &[&str] = match choice {
            ChangedAnswer::Recreate | ChangedAnswer::Reinstall => &["alpha"],
            ChangedAnswer::KeepOld => &[],
        };
        assert_eq!(ran(), installed, "{choice:?}");
        assert_eq!(questions().len(), 1, "{choice:?}");
        let state = project.state();
        assert_eq!(
            state.packs["alpha"].status,
            PackStatus::Installed,
            "{choice:?}"
        );
        assert_eq!(state.disk, project.disk_id(), "{choice:?}");
    }
}

/// Test that a template build from a base template with a changed pack
/// starts with a new disk. The build has `--yes`, so the answer is
/// "Re-create".
///   1. Build a template with alpha
///   2. Build a second template from it with another arg of alpha
///   3. Check that alpha installs again and that the install snapshot of
///      the second template has the new arg
#[test]
fn template_from_base_template_with_changed_pack_re_creates_disk() {
    let project = with_base_template();
    let derived = "[packs]\nalpha = { version = 1, args = { mode = \"slow\" } }\n";
    project
        .build_template(
            "derived",
            &template_config(&project, derived, Some("base")),
            false,
        )
        .unwrap();
    assert_eq!(ran(), ["alpha"]);
    let template = crate::templates::find(&project.context().data, "derived").unwrap();
    let dir = PinnedDir::pin(template.dir.to_path().unwrap()).unwrap();
    let snapshot = snapshots::read(&dir)
        .get(&alpha_fingerprint(derived))
        .cloned()
        .unwrap();
    assert_eq!(snapshot.args["mode"], "slow");
}
