//! Tests of the user packs (`~name`): the load from the user pack
//! directories on the disk, the checks of the pack files, and the config
//! entries that name a user pack.

use std::os::unix::fs::symlink;
use std::path::Path;

use crate::config::{ConfigOverrides, LayeredConfig, ResolveInput};
use crate::packs::PackManager;
use crate::settings::PackSettings;
use crate::test_cfg::oci::{data_dir_at, verified_dir};
use crate::test_cfg::packs::{resolve_with, with_user_packs, write_user_pack};
use crate::test_cfg::{block_on, temp_dir};

/// `pack.toml` of the user pack `hello`, with the choice arg `greeting`.
const HELLO_PACK: &str = r#"
label = "Hello"
description = "Says hello"
kind = "tool"

[[args]]
key = "greeting"
type = "choice"
description = "Greeting"
default = "hi"
values = ["hi", "hello"]
"#;

/// `config.lua` of the user pack `hello`. It writes the pack values into
/// the env.
const HELLO_CONFIG: &str = r"
config.env = {
    HELLO_GREETING = pack.args.greeting,
    HELLO_NAME = pack.name,
    HELLO_VERSION = tostring(pack.version),
    HELLO_DIR = pack.directory,
}
";

/// Write the user pack `hello` into `dir`.
fn write_hello(dir: &Path) {
    write_user_pack(
        dir,
        "hello",
        &[
            ("pack.toml", HELLO_PACK),
            ("config.lua", HELLO_CONFIG),
            ("setup.sh", "true\n"),
        ],
    );
}

/// The load error (`{:#}`) of the user packs of `dirs`. Panics if the
/// packs load.
fn load_error(dirs: &[&Path]) -> String {
    match with_user_packs(dirs, &[]) {
        Ok(_) => panic!("the user packs load"),
        Err(e) => format!("{e:#}"),
    }
}

/// The resolve error (`{:#}`) of `toml` with `packs`. Panics if the config
/// resolves.
fn resolve_error(packs: &PackManager, toml: &str) -> String {
    match resolve_with(packs, toml) {
        Ok(_) => panic!("config resolves:\n{toml}"),
        Err(e) => format!("{e:#}"),
    }
}

/// Test that a user pack in a settings directory loads and resolves with
/// its `config.lua` and setup script. This is the main flow of user packs.
///   1. Write the pack `hello`, a hidden file and an editor backup into
///      `~/packs`, and set `packs.dirs = ["~/packs"]`
///   2. Load the packs and resolve `"~hello" = { args = { … } }`
///   3. Check the id, the version, the arg, the env from `config.lua`, the
///      mount directory `local.hello` and the installer
///   4. Change a pack file and check that the content hash changes
#[test]
fn user_pack_from_settings_dir_resolves_with_config_lua_and_setup() {
    let home = temp_dir();
    let dir = home.path().join("packs");
    write_hello(&dir);
    write_user_pack(
        &dir,
        "hello",
        &[(".DS_Store", "x"), ("config.lua~", "error()")],
    );
    let settings = PackSettings {
        dirs: Some(vec!["~/packs".to_string()]),
        defaults: vec![],
    };
    let packs = crate::packs::init(&settings, &verified_dir(home.path())).unwrap();

    let toml = "[packs]\n\"~hello\" = { args = { greeting = \"hello\" } }\n";
    let layers = LayeredConfig::from_values(
        vec![],
        None,
        vec![("airlock.toml", toml::from_str(toml).unwrap())],
    )
    .unwrap();
    let data = temp_dir();
    let resolved = block_on(layers.resolve(&ResolveInput {
        data_dir: &data_dir_at(data.path()),
        packs: &packs,
        overrides: &ConfigOverrides::default(),
        project_root: None,
        approval: crate::packs::approval::ApprovalMode::APPROVE,
    }))
    .unwrap();
    let hello = &resolved.packs[0];
    assert_eq!(hello.metadata().id, "~hello");
    assert_eq!(hello.metadata().version, None);
    let env = &resolved.values.env;
    assert_eq!(env["HELLO_GREETING"].value, "hello");
    assert_eq!(env["HELLO_NAME"].value, "~hello");
    assert_eq!(env["HELLO_VERSION"].value, "nil");
    assert!(
        env["HELLO_DIR"].value.ends_with("/packs/local.hello"),
        "{}",
        env["HELLO_DIR"].value
    );
    assert!(Path::new(&env["HELLO_DIR"].value).is_dir());
    let installer = hello.setup_installer().unwrap();
    assert_eq!(installer.pack, "~hello");
    assert!(installer.script.contains("true\n"));

    let hash = |packs: &PackManager| {
        *packs
            .offered()
            .iter()
            .find(|p| p.metadata().id == "~hello")
            .unwrap()
            .content_hash()
            .unwrap()
    };
    let before = hash(&packs);
    std::fs::write(dir.join("hello/setup.sh"), "true\ntrue\n").unwrap();
    let packs = crate::packs::init(&settings, &verified_dir(home.path())).unwrap();
    assert_ne!(hash(&packs), before);
}

/// Test that a user pack entry with `version`, an entry with a bad user
/// pack name and an entry of a user pack that does not exist are refused.
/// The stored config of the last run (`config --resolved`) gives the same
/// error.
///   1. Load the pack `hello` from a directory
///   2. Resolve `"~hello" = { version = "1" }` and check the error
///   3. Resolve `"~../hello" = {}` and check the user pack name error
///   4. Resolve `"~nope" = {}` and check the unknown user pack error
///   5. Store and load the layered config as `start` does, resolve it and
///      check the same error
#[test]
fn user_pack_entry_with_version_bad_name_or_missing_pack_is_refused() {
    let tmp = temp_dir();
    write_hello(tmp.path());
    let (packs, _) = with_user_packs(&[tmp.path()], &[]).unwrap();

    let error = resolve_error(&packs, "[packs]\n\"~hello\" = { version = \"1\" }\n");
    assert!(
        error.contains("`packs.\"~hello\".version` is not allowed"),
        "{error}"
    );

    let error = resolve_error(&packs, "[packs]\n\"~../hello\" = {}\n");
    assert!(error.contains("not a valid user pack name"), "{error}");

    let toml = "[packs]\n\"~nope\" = {}\n";
    let error = resolve_error(&packs, toml);
    assert!(
        error.contains("`packs.\"~nope\"` unknown user pack"),
        "{error}"
    );

    let layers = LayeredConfig::from_values(
        vec![],
        None,
        vec![("airlock.toml", toml::from_str(toml).unwrap())],
    )
    .unwrap();
    let stored: LayeredConfig =
        serde_json::from_value(serde_json::to_value(&layers).unwrap()).unwrap();
    let data = temp_dir();
    let error = block_on(stored.resolve(&ResolveInput {
        data_dir: &data_dir_at(data.path()),
        packs: &packs,
        overrides: &ConfigOverrides::default(),
        project_root: None,
        approval: crate::packs::approval::ApprovalMode::APPROVE,
    }))
    .err()
    .map(|e| format!("{e:#}"))
    .unwrap();
    assert!(
        error.contains("`packs.\"~nope\"` unknown user pack"),
        "{error}"
    );
}

/// Test that a known pack file must be a regular file, and that other
/// entries in a pack directory are ignored. A link can point out of the
/// directory that the user picked. Airlock reads only the known files, so
/// many other files cannot make the load slow.
///   1. Link `config.lua` of a pack to a file out of the directory and
///      check the error
///   2. Make `setup.sh` a directory and check the error
///   3. Put a sub-directory and an unknown file in a pack and check that
///      the pack loads
#[test]
fn links_in_user_pack_are_refused_and_unknown_entries_ignored() {
    let tmp = temp_dir();
    let outside = tmp.path().join("outside.lua");
    std::fs::write(&outside, HELLO_CONFIG).unwrap();

    let dir = tmp.path().join("linked-file");
    write_user_pack(&dir, "hello", &[("pack.toml", HELLO_PACK)]);
    symlink(&outside, dir.join("hello/config.lua")).unwrap();
    let error = load_error(&[&dir]);
    assert!(
        error.contains("hello/config.lua: not a regular file"),
        "{error}"
    );

    let dir = tmp.path().join("setup-dir");
    write_user_pack(&dir, "hello", &[("pack.toml", HELLO_PACK)]);
    std::fs::create_dir(dir.join("hello/setup.sh")).unwrap();
    let error = load_error(&[&dir]);
    assert!(
        error.contains("hello/setup.sh: not a regular file"),
        "{error}"
    );

    let dir = tmp.path().join("extra");
    write_hello(&dir);
    std::fs::create_dir(dir.join("hello/lib")).unwrap();
    std::fs::write(dir.join("hello/README.md"), "x").unwrap();
    with_user_packs(&[&dir], &[]).unwrap();
}

/// Test that stray entries in a user pack directory do not stop the load.
/// A user can keep a README or a link there, and a bad entry must not
/// break every command. Links and bad directory names give a warning, so
/// the user knows why a pack is not there.
///   1. Write the pack `hello`, a file, a hidden directory, a directory
///      with a name that is not a pack name, and a link to a pack directory
///   2. Check that only `hello` loads
///   3. Check the warnings for the bad name and the link
#[test]
fn stray_entries_in_user_pack_dir_are_skipped() {
    let tmp = temp_dir();
    let dir = tmp.path().join("packs");
    write_hello(&dir);
    std::fs::write(dir.join("README.md"), "My packs\n").unwrap();
    std::fs::create_dir(dir.join(".git")).unwrap();
    write_user_pack(&dir, "Bad_Name", &[("pack.toml", HELLO_PACK)]);
    write_hello(&tmp.path().join("real"));
    symlink(tmp.path().join("real/hello"), dir.join("linked")).unwrap();

    let (packs, warnings) = with_user_packs(&[&dir], &[]).unwrap();
    let local: Vec<String> = packs
        .offered()
        .iter()
        .map(|p| p.metadata().id.to_string())
        .filter(|id| id.starts_with('~'))
        .collect();
    assert_eq!(local, ["~hello"]);
    assert_eq!(warnings.len(), 2, "{warnings:?}");
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("Bad_Name: skipped: not a valid pack name")),
        "{warnings:?}"
    );
    assert!(
        warnings
            .iter()
            .any(|w| w.contains("linked: skipped: links are not allowed")),
        "{warnings:?}"
    );
}

/// Test that the same pack in two user pack directories is refused. Else
/// the pack that the config gets would depend on the directory order.
///   1. Write the pack `hello` into two directories
///   2. Check that the error names the pack and both directories
#[test]
fn same_user_pack_in_two_dirs_is_refused() {
    let tmp = temp_dir();
    let (first, second) = (tmp.path().join("a"), tmp.path().join("b"));
    write_hello(&first);
    write_hello(&second);
    let error = load_error(&[&first, &second]);
    assert!(
        error.contains("pack ~hello is in two user pack directories"),
        "{error}"
    );
    assert!(
        error.contains(&first.join("hello").display().to_string()),
        "{error}"
    );
    assert!(
        error.contains(&second.join("hello").display().to_string()),
        "{error}"
    );
}

/// Test the size and count limits of user packs. The limits keep a wrong
/// directory (for example the home directory) from a slow start.
///   1. Write a pack with a file larger than 256 KiB and check the error
///   2. Write a setup script of the maximum size and check that it loads.
///      Add one byte and check the error. A larger script does not fit in
///      the install exec.
///   3. Write 65 packs into one directory and check the count error
#[test]
fn user_packs_over_limits_are_refused() {
    let tmp = temp_dir();
    let big = "x".repeat(256 * 1024 + 1);
    let dir = tmp.path().join("big-file");
    write_user_pack(
        &dir,
        "hello",
        &[("pack.toml", HELLO_PACK), ("setup.sh", &big)],
    );
    let error = load_error(&[&dir]);
    assert!(
        error.contains("hello/setup.sh: the file is larger than 262144 bytes"),
        "{error}"
    );

    let max = crate::packs::install::compose::max_setup_len();
    let dir = tmp.path().join("big-setup");
    let setup = "x".repeat(max);
    write_user_pack(
        &dir,
        "hello",
        &[("pack.toml", HELLO_PACK), ("setup.sh", &setup)],
    );
    with_user_packs(&[&dir], &[]).unwrap();
    write_user_pack(&dir, "hello", &[("setup.sh", &format!("{setup}x"))]);
    let error = load_error(&[&dir]);
    assert!(
        error.contains(&format!(
            "hello/setup.sh: the file is too large (limit {max} bytes)"
        )),
        "{error}"
    );

    let dir = tmp.path().join("many");
    for i in 0..65 {
        write_user_pack(&dir, &format!("p{i}"), &[("pack.toml", HELLO_PACK)]);
    }
    let error = load_error(&[&dir]);
    assert!(error.contains("more than 64 packs"), "{error}");
}

/// Test the user pack directories of the settings. A missing directory
/// gives a warning only when the user set it: the default directory is
/// optional. A relative directory is refused, because it would depend on
/// the current directory.
///   1. Load from a set directory that does not exist and check the warning
///   2. Get the default directories of empty settings and check that a
///      missing default directory gives no warning
///   3. Check that `~/packs` expands to the home directory
///   4. Check that a relative directory is refused
#[test]
fn user_pack_dir_settings_expand_home_and_warn_only_for_set_missing_dir() {
    let tmp = temp_dir();
    let missing = tmp.path().join("missing");
    let (_, warnings) = with_user_packs(&[&missing], &[]).unwrap();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains(&missing.display().to_string()),
        "{warnings:?}"
    );

    let dirs = PackSettings::default()
        .user_dirs(&verified_dir(tmp.path()))
        .unwrap();
    let (_, warnings) = crate::packs::init_builtin()
        .unwrap()
        .with_user_packs(&dirs, vec![])
        .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");

    let home = verified_dir(tmp.path());
    let settings = |dir: &str| PackSettings {
        dirs: Some(vec![dir.to_string()]),
        defaults: vec![],
    };
    let dirs = settings("~/packs").user_dirs(&home).unwrap();
    assert_eq!(dirs[0].0.to_string(), format!("{home}/packs"));
    assert!(dirs[0].1);
    let error = format!("{:#}", settings("packs").user_dirs(&home).unwrap_err());
    assert!(error.contains("must be an absolute path: packs"), "{error}");
}
