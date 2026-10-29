//! Tests of the project packs (`./path`): the load from a directory of the
//! project, the checks of the path, and the config files that can name a
//! project pack.

use std::fmt::Write as _;
use std::os::unix::fs::symlink;

use crate::config::{ConfigOverrides, LayeredConfig};
use crate::packs::PackSource;
use crate::test_cfg::config::{ConfigDirs, resolve_layers_in};
use crate::test_cfg::packs::write_user_pack;

/// `pack.toml` of the project pack `packs/setup`, with the choice arg
/// `greeting`.
const SETUP_PACK: &str = r#"
label = "Setup"
description = "Project setup"

[[args]]
key = "greeting"
type = "choice"
description = "Greeting"
default = "hi"
values = ["hi", "hello"]
"#;

/// `config.lua` of the project pack `packs/setup`. It writes the pack
/// values into the env.
const SETUP_CONFIG: &str = r"
config.env = {
    SETUP_GREETING = pack.args.greeting,
    SETUP_NAME = pack.name,
    SETUP_VERSION = tostring(pack.version),
    SETUP_DIR = pack.directory,
}
";

/// Make a project with the project pack `packs/setup`.
fn project_with_setup_pack() -> ConfigDirs {
    let dirs = ConfigDirs::new();
    write_user_pack(
        &dirs.project().join("packs"),
        "setup",
        &[
            ("pack.toml", SETUP_PACK),
            ("config.lua", SETUP_CONFIG),
            ("setup.sh", "true\n"),
        ],
    );
    dirs
}

/// Test that a project pack in a directory of the project resolves with its
/// `config.lua` and setup script. This is the main flow of project packs.
/// The local project file in the project root can also set its args.
///   1. Write the pack `packs/setup` into the project
///   2. Name it in `airlock.toml`, and set an arg in `airlock.local.toml`
///   3. Resolve and check the id, the version, the source, the env from
///      `config.lua`, the mount directory `project.<hash>` and the
///      installer
///   4. Resolve the same pack path in a second project and check that it
///      gets a different mount directory
#[test]
fn project_pack_in_project_file_resolves_with_config_lua_and_setup() {
    let dirs = project_with_setup_pack();
    dirs.project_file("airlock.toml", "[packs]\n\"./packs/setup\" = {}\n");
    dirs.project_file(
        "airlock.local.toml",
        "[packs]\n\"./packs/setup\" = { args = { greeting = \"hello\" } }\n",
    );

    let resolved = dirs.resolve().unwrap();
    let setup = &resolved.packs[0];
    let metadata = setup.metadata();
    assert_eq!(metadata.id, "./packs/setup");
    assert_eq!(metadata.version, None);
    assert!(matches!(metadata.source, PackSource::Project { .. }));
    let env = &resolved.values.env;
    assert_eq!(env["SETUP_GREETING"].value, "hello");
    assert_eq!(env["SETUP_NAME"].value, "./packs/setup");
    assert_eq!(env["SETUP_VERSION"].value, "nil");
    let dir_name = metadata.dir_name();
    let hash = dir_name.strip_prefix("project.").unwrap();
    assert!(
        hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()),
        "{dir_name}"
    );
    assert!(
        env["SETUP_DIR"]
            .value
            .ends_with(&format!("/packs/{dir_name}")),
        "{}",
        env["SETUP_DIR"].value
    );
    let installer = setup.setup_installer().unwrap();
    assert_eq!(installer.pack, "./packs/setup");
    assert!(installer.script.contains("true\n"));

    let other = project_with_setup_pack();
    other.project_file("airlock.toml", "[packs]\n\"./packs/setup\" = {}\n");
    let resolved = other.resolve().unwrap();
    assert_ne!(resolved.packs[0].metadata().dir_name(), dir_name);
}

/// Test that a project pack path that can go out of the project is
/// refused. The guest can write the project, so it can put links there.
///   1. Name `./../setup` and `./packs/../packs/setup` and check the path
///      error
///   2. Link a directory of the project to `packs` and name the pack through
///      the link: check the error
///   3. Link `packs/alias` to `packs/setup` and name `./packs/alias`:
///      check the error
#[test]
fn project_pack_path_with_dotdot_or_link_is_refused() {
    let dirs = project_with_setup_pack();
    for key in ["./../setup", "./packs/../packs/setup", "./packs//setup"] {
        dirs.project_file("airlock.toml", &format!("[packs]\n\"{key}\" = {{}}\n"));
        let error = dirs.error();
        assert!(
            error.contains("not a valid project pack path"),
            "{key}: {error}"
        );
    }

    symlink(dirs.project().join("packs"), dirs.project().join("linked")).unwrap();
    dirs.project_file("airlock.toml", "[packs]\n\"./linked/setup\" = {}\n");
    let error = dirs.error();
    assert!(
        error.contains("cannot read project pack ./linked/setup"),
        "{error}"
    );

    symlink(
        dirs.project().join("packs/setup"),
        dirs.project().join("packs/alias"),
    )
    .unwrap();
    dirs.project_file("airlock.toml", "[packs]\n\"./packs/alias\" = {}\n");
    let error = dirs.error();
    assert!(
        error.contains("project pack ./packs/alias is not a directory"),
        "{error}"
    );
}

/// Test that the local project file can name a project pack, and a user
/// file cannot. A user file applies to all projects and cannot have
/// `[packs]`.
///   1. Name the pack in `~/.airlock/airlock.toml` and check the error
///   2. Name the pack in `.airlock/airlock.toml` and check that it resolves
#[test]
fn project_pack_in_local_file_resolves_and_in_user_file_is_refused() {
    let dirs = project_with_setup_pack();
    let toml = "[packs]\n\"./packs/setup\" = {}\n";
    dirs.user_file(".airlock/airlock.toml", toml);
    let error = format!("{:#}", dirs.load().err().unwrap());
    assert!(
        error.contains("`[packs]` is allowed only in project config files"),
        "{error}"
    );

    std::fs::remove_file(dirs.home().join(".airlock/airlock.toml")).unwrap();
    dirs.project_file(".airlock/airlock.toml", toml);
    let resolved = dirs.resolve().unwrap();
    assert_eq!(resolved.packs[0].metadata().id, "./packs/setup");
}

/// Test that a missing project pack, a project pack with no project, and
/// too many project packs are refused. A template build has no project.
/// The guest can edit the project files, so the number of packs that load
/// from the disk is limited.
///   1. Name `./packs/nope` and check that the error names the path
///   2. Resolve `./packs/setup` with no project root and check the error
///   3. Name 33 project packs and check the limit error
#[test]
fn missing_project_pack_or_no_project_is_refused() {
    let dirs = project_with_setup_pack();
    dirs.project_file("airlock.toml", "[packs]\n\"./packs/nope\" = {}\n");
    let error = dirs.error();
    assert!(
        error.contains("project pack ./packs/nope not found"),
        "{error}"
    );

    dirs.project_file("airlock.toml", "[packs]\n\"./packs/setup\" = {}\n");
    let error = resolve_layers_in(&dirs.load().unwrap(), ConfigOverrides::default(), None)
        .err()
        .map(|e| format!("{e:#}"))
        .unwrap();
    assert!(
        error.contains("project packs are not supported in templates"),
        "{error}"
    );

    let mut toml = "[packs]\n".to_string();
    for n in 0..33 {
        let _ = writeln!(toml, "\"./packs/p{n}\" = {{}}");
    }
    dirs.project_file("airlock.toml", &toml);
    let error = dirs.error();
    assert!(error.contains("too many project packs"), "{error}");
}

/// Test that the stored config of the last run resolves a project pack
/// again. The config change check resolves the stored config.
///   1. Store and load the config with a project pack
///   2. Resolve it and check the pack
#[test]
fn stored_config_with_project_pack_resolves() {
    let dirs = project_with_setup_pack();
    dirs.project_file("airlock.toml", "[packs]\n\"./packs/setup\" = {}\n");
    let project = dirs.project_dir();
    let stored = serde_json::to_value(dirs.load().unwrap()).unwrap();
    let layers: LayeredConfig = serde_json::from_value(stored).unwrap();
    let resolved = resolve_layers_in(&layers, ConfigOverrides::default(), Some(&project)).unwrap();
    assert_eq!(resolved.packs[0].metadata().id, "./packs/setup");
}
