//! Tests of the config change check of `airlock start`.

use crate::sandbox::read_run_meta;
use crate::sandbox::start::Exit;
use crate::test_cfg::packs::test_packs;
use crate::test_cfg::start::{StartProject, fake_host};

/// The config of the first start.
const FIRST: &str = "[vm]\ncpus = 2\n";
/// [`FIRST`] with a changed value.
const CHANGED: &str = "[vm]\ncpus = 3\n";

/// Take the number of image preparations since the last call.
fn prepared() -> usize {
    fake_host(|host| std::mem::take(&mut host.prepared))
}

/// Test that a changed config stops a start without a terminal and with
/// `--yes`, before the image step. A start from code in the sandbox or from
/// a script must not use a config that nobody accepted.
///   1. Start a new sandbox, then start again with the same config
///   2. Start with a changed config without and with `--yes`, and check
///      exit code 2 and no image step
///   3. Start with the changed config, `--yes` and
///      `--allow-configuration-changes`, and check that it starts
///   4. Start with the changed config without the flags, and check that it
///      starts because the copy is now of the changed config
#[test]
fn changed_config_stops_start_until_allowed() {
    let project = StartProject::new(test_packs());
    project.start(FIRST, false).unwrap();
    project.start(FIRST, false).unwrap();
    prepared();

    for yes in [false, true] {
        assert!(matches!(project.start(CHANGED, yes), Err(Exit::Code(2))));
    }
    assert_eq!(prepared(), 0);
    project.start_with(CHANGED, true, true).unwrap();
    project.start(CHANGED, false).unwrap();
}

/// Test that a sandbox with a disk but no config copy stops a start without
/// a terminal. Nobody accepted the config that made the disk, for example
/// in an imported sandbox of an older airlock.
///   1. Start a sandbox, check that it has a disk, then remove its config
///      copy
///   2. Start without a terminal and check exit code 2
///   3. Start with `--allow-configuration-changes` and check that it
///      starts and gets a copy
///   4. Start again without the flag and check that it starts
#[test]
fn sandbox_with_disk_without_config_copy_stops_start_until_allowed() {
    let project = StartProject::new(test_packs());
    project.start(FIRST, false).unwrap();
    assert!(project.sandbox_dir().join("disk.img").is_file());
    std::fs::remove_file(project.sandbox_dir().join("run.json")).unwrap();

    assert!(matches!(project.start(FIRST, false), Err(Exit::Code(2))));
    project.start_with(FIRST, false, true).unwrap();
    assert!(
        read_run_meta(&project.sandbox().unwrap().dir)
            .last_run_config
            .is_some()
    );
    project.start(FIRST, false).unwrap();
}

/// Test that a new sandbox starts without a terminal and without a
/// question. It has no disk yet, thus no config made its state.
///   1. Start a new project without a terminal and check that it starts
///   2. Check that the sandbox has a config copy and a disk now
#[test]
fn new_sandbox_without_disk_starts_without_question() {
    let project = StartProject::new(test_packs());
    project.start(FIRST, false).unwrap();
    assert!(
        read_run_meta(&project.sandbox().unwrap().dir)
            .last_run_config
            .is_some()
    );
    assert!(project.sandbox_dir().join("disk.img").is_file());
}

/// Test that a mount source symlink that points to an other dir since the
/// last start stops a start without a terminal. Code in the sandbox can
/// change the symlink in the project dir. A `run.json` of an older airlock
/// without host paths gives no stop.
///   1. Make the dirs `a` and `b` and the symlink `data` to `a` in the
///      project, and start two times with a mount of `data`
///   2. Change the symlink to `b` and check exit code 2
///   3. Remove the host paths from `run.json` and check that the start
///      runs
#[test]
fn retargeted_mount_source_symlink_stops_start() {
    let project = StartProject::new(test_packs());
    let dir = project.project_dir();
    std::fs::create_dir(dir.join("a")).unwrap();
    std::fs::create_dir(dir.join("b")).unwrap();
    std::os::unix::fs::symlink("a", dir.join("data")).unwrap();
    let toml = "[mounts.data]\nsource = \"./data\"\ntarget = \"/data\"\n";
    project.start(toml, false).unwrap();
    project.start(toml, false).unwrap();

    std::fs::remove_file(dir.join("data")).unwrap();
    std::os::unix::fs::symlink("b", dir.join("data")).unwrap();
    assert!(matches!(project.start(toml, false), Err(Exit::Code(2))));

    let run_json = project.sandbox_dir().join("run.json");
    let mut meta: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&run_json).unwrap()).unwrap();
    meta.as_object_mut()
        .unwrap()
        .remove("last_run_paths")
        .unwrap();
    std::fs::write(&run_json, meta.to_string()).unwrap();
    project.start(toml, false).unwrap();
}

/// Test that a rule change that the user approved in the sandbox does not
/// give the config question at the next start, for TOML, JSON and YAML
/// project files. The test uses the real files: the start loads them from
/// the project dir, and the rule write changes them there. Thus the origin
/// of the file and the sandbox dir must agree between the two. Other changes
/// of the same file still give the question.
///   1. Write the project file and start a sandbox
///   2. Write a new rule into the file and approve it, then start without
///      a terminal and check that it starts
///   3. Change the allow list of the rule, approve it, and start again
///   4. Change an other value in the file and check exit code 2
#[test]
fn approved_rule_change_in_project_file_does_not_ask_again() {
    use crate::config::LayeredConfig;
    use crate::config::config_values::RuleEnabled;
    use crate::network::rule_file::{ApprovedRules, RuleWrite, write_rule};
    use crate::sandbox::start::config_change::approve_rule;
    use crate::test_cfg::oci::verified_dir;
    use crate::test_cfg::temp_dir;

    for (file, first) in [
        ("airlock.toml", FIRST),
        ("airlock.json", "{\"vm\": {\"cpus\": 2}}\n"),
        ("airlock.yaml", "vm:\n  cpus: 2\n"),
    ] {
        let project = StartProject::new(test_packs());
        let path = project.project_dir().join(file);
        std::fs::write(&path, first).unwrap();
        let project_dir = verified_dir(&project.project_dir());
        // The local project file dir is outside the project and empty.
        let local = temp_dir();
        let load = || {
            LayeredConfig::load_from(
                &project.context().home,
                &project_dir,
                &verified_dir(local.path()),
            )
            .unwrap()
        };
        project.start_config(&load(), false, false).unwrap();
        let sandbox_dir = project.sandbox().unwrap().dir;

        for allow in ["registry.npmjs.org", "*.npmjs.org"] {
            let rule = RuleWrite {
                name: "npm".into(),
                enabled: Some(RuleEnabled::OnDemand),
                description: Some("npm packages".into()),
                allow: Some(vec![allow.into()]),
                deny: None,
            };
            let mut approved = ApprovedRules::load(&sandbox_dir, &project_dir).unwrap();
            let (origin, table) = write_rule(&project_dir, &rule, &mut approved).unwrap();
            approve_rule(&sandbox_dir, &origin, &rule.name, table).unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            assert!(text.contains(allow), "{file}: {text}");
            project
                .start_config(&load(), false, false)
                .unwrap_or_else(|e| panic!("{file}: {e:?}"));
        }

        let text = std::fs::read_to_string(&path).unwrap();
        std::fs::write(&path, text.replace('2', "3")).unwrap();
        assert!(
            matches!(
                project.start_config(&load(), false, false),
                Err(Exit::Code(2))
            ),
            "{file}"
        );
    }
}

/// Test that a rule change is refused when the sandbox put keys into the
/// rule in the project file, for TOML, JSON and YAML. The sandbox can write
/// the file. The approval of a harmless change must not approve the planted
/// keys for the next start, for example `inject` of a secret with a host
/// that the sandbox controls.
///   1. Write the project file with the rule `npm` and start a sandbox
///   2. Put `inject` and one more allow host into `npm`, and a new rule
///      `evil` with `inject`
///   3. Write a new description of `npm`, and write the rule `evil`. Check
///      that both writes are refused
///   4. Check that the file and the config of the last run did not change
///   5. Start without a terminal and check exit code 2
#[test]
fn rule_change_with_keys_planted_in_project_file_is_refused() {
    use crate::config::LayeredConfig;
    use crate::messages::grants as msg;
    use crate::network::rule_file::{ApprovedRules, RuleWrite, write_rule};
    use crate::test_cfg::oci::verified_dir;
    use crate::test_cfg::temp_dir;

    let toml_env = "[env]\nGITHUB_TOKEN = { value = \"x\", mask = true }\n\n";
    let json_env = r#""env": {"GITHUB_TOKEN": {"value": "x", "mask": true}}"#;
    let yaml_env = "env:\n  GITHUB_TOKEN:\n    value: x\n    mask: true\n";
    for (file, first, planted) in [
        (
            "airlock.toml",
            format!("{toml_env}[network.rules.npm]\nallow = [\"registry.npmjs.org\"]\n"),
            format!(
                "{toml_env}[network.rules.npm]\n\
                 allow = [\"registry.npmjs.org\", \"attacker.example.com\"]\n\
                 inject = [\"GITHUB_TOKEN\"]\n\n\
                 [network.rules.evil]\ninject = [\"GITHUB_TOKEN\"]\n"
            ),
        ),
        (
            "airlock.json",
            format!(
                r#"{{{json_env}, "network": {{"rules": {{"npm": {{"allow": ["registry.npmjs.org"]}}}}}}}}"#
            ),
            format!(
                r#"{{{json_env}, "network": {{"rules": {{
                    "npm": {{"allow": ["registry.npmjs.org", "attacker.example.com"],
                             "inject": ["GITHUB_TOKEN"]}},
                    "evil": {{"inject": ["GITHUB_TOKEN"]}}}}}}}}"#
            ),
        ),
        (
            "airlock.yaml",
            format!("{yaml_env}network:\n  rules:\n    npm:\n      allow: [registry.npmjs.org]\n"),
            format!(
                "{yaml_env}network:\n  rules:\n    npm:\n      \
                 allow: [registry.npmjs.org, attacker.example.com]\n      \
                 inject: [GITHUB_TOKEN]\n    evil:\n      inject: [GITHUB_TOKEN]\n"
            ),
        ),
    ] {
        let project = StartProject::new(test_packs());
        let path = project.project_dir().join(file);
        std::fs::write(&path, &first).unwrap();
        let project_dir = verified_dir(&project.project_dir());
        // The local project file dir is outside the project and empty.
        let local = temp_dir();
        let load = || {
            LayeredConfig::load_from(
                &project.context().home,
                &project_dir,
                &verified_dir(local.path()),
            )
            .unwrap()
        };
        project
            .start_config(&load(), false, false)
            .unwrap_or_else(|e| panic!("{file}: {e:?}"));
        let sandbox_dir = project.sandbox().unwrap().dir;
        let run_json = project.sandbox_dir().join("run.json");
        let stored = std::fs::read_to_string(&run_json).unwrap();
        // The grant context of the session reads the approved rules at the
        // start, before the sandbox can change the file.
        let mut approved = ApprovedRules::load(&sandbox_dir, &project_dir).unwrap();
        std::fs::write(&path, &planted).unwrap();

        let origin = path.display().to_string();
        for rule in [
            RuleWrite {
                name: "npm".into(),
                description: Some("harmless".into()),
                ..RuleWrite::default()
            },
            RuleWrite {
                name: "evil".into(),
                allow: Some(vec!["evil.example.com".into()]),
                ..RuleWrite::default()
            },
        ] {
            let err = write_rule(&project_dir, &rule, &mut approved).unwrap_err();
            assert_eq!(
                format!("{err:#}"),
                msg::rule_not_approved(&rule.name, &origin),
                "{file}"
            );
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), planted, "{file}");
        assert_eq!(
            std::fs::read_to_string(&run_json).unwrap(),
            stored,
            "{file}"
        );
        assert!(
            matches!(
                project.start_config(&load(), false, false),
                Err(Exit::Code(2))
            ),
            "{file}"
        );
    }
}
