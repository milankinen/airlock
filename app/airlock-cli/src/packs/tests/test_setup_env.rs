//! Tests of the setup env: `config.lua` gives values to the setup script.
//! The values can be secrets, so the tests also check that no error text
//! and no debug output shows them, and that the install fingerprint
//! follows them.

use std::path::Path;

use indicatif::ProgressBar;
use sha2::{Digest, Sha256};

use crate::packs::install::progress::InstallProgress;
use crate::packs::install::setup::install_loop;
use crate::packs::install::state::InstallState;
use crate::packs::{EnvMacKey, PackManager, SetupEnv};
use crate::test_cfg::oci::data_dir_at;
use crate::test_cfg::packs::{
    HostExec, load_packs, resolve_in, resolve_with, with_user_packs, write_user_pack,
};
use crate::test_cfg::{block_on_local, temp_dir};

/// A value that no error text and no debug output may show.
const SECRET: &str = "s3cret-value";

/// `pack.toml` of the test packs.
const PACK_TOML: &str = "label = \"Vault\"\ndescription = \"Gets a token\"\n";

/// Setup script that exits 0 only if it gets the values of
/// [`token_config`] and [`SECRET`].
const VAULT_SETUP: &str = r#"
[ "$AIRLOCK_PACK_ENV_TOKEN" = "$(cat "$AIRLOCK_PACK_ENV_TOKEN_FILE")" ] || exit 3
[ "$AIRLOCK_PACK_ENV_PLAIN" = "s3cret-value" ] || exit 4
"#;

/// `config.lua` that reads `TOKEN` from the file `token_file` and sets
/// `PLAIN` to [`SECRET`].
fn token_config(token_file: &Path) -> String {
    format!(
        r#"
local file = assert(io.open("{path}"))
setup.env.TOKEN = file:read("a")
file:close()
setup.env.TOKEN_FILE = "{path}"
setup.env.PLAIN = "{SECRET}"
"#,
        path = token_file.display()
    )
}

/// The built-in test pack `envy@1` with `config_lua`, and a setup script
/// if `setup` is true.
fn envy_packs(config_lua: &str, setup: bool) -> PackManager {
    let mut files = vec![
        ("envy@1/pack.toml", PACK_TOML.to_string()),
        ("envy@1/config.lua", config_lua.to_string()),
    ];
    if setup {
        files.push(("envy@1/setup.sh", "true\n".to_string()));
    }
    load_packs(files)
}

/// The resolve error (`{:#}`) of the pack `envy@1` with `config_lua`.
fn envy_error(config_lua: &str, setup: bool) -> String {
    match resolve_with(
        &envy_packs(config_lua, setup),
        "[packs]\nenvy = { version = 1 }\n",
    ) {
        Ok(_) => panic!("config resolves:\n{config_lua}"),
        Err(e) => format!("{e:#}"),
    }
}

/// Test that the setup script of a user pack gets the setup env of its
/// `config.lua`. This is the main flow of the setup env. The values must
/// reach the script without change, also with quotes, `$` and new lines.
///   1. Write the user pack `vault` whose `config.lua` reads a token file
///   2. Resolve the config and make the installers
///   3. Run the install loop on the host and check that the script exit
///      code tells that it got the values
#[test]
fn setup_env_from_config_lua_reaches_setup_script() {
    let tmp = temp_dir();
    let token = tmp.path().join("token.txt");
    std::fs::write(&token, "line \"one\" $HOME\nline two").unwrap();
    let dir = tmp.path().join("packs");
    write_user_pack(
        &dir,
        "vault",
        &[
            ("pack.toml", PACK_TOML),
            ("config.lua", &token_config(&token)),
            ("setup.sh", VAULT_SETUP),
        ],
    );
    let (packs, _) = with_user_packs(&[&dir], &[]).unwrap();
    let resolved = resolve_with(&packs, "[packs]\n\"~vault\" = {}\n").unwrap();
    let installers = crate::start::install::install_candidates(&resolved.packs);

    let mut exec = HostExec::new();
    let mut state = InstallState::default();
    let mut progress = InstallProgress::new(ProgressBar::hidden(), None, false);
    let outcome = block_on_local(install_loop(
        &mut exec,
        &installers,
        &mut state,
        &mut |_| Ok(()),
        &mut progress,
    ))
    .unwrap();
    progress.finish();
    assert!(outcome.failed.is_empty(), "{:?}", outcome.failed);
    assert_eq!(outcome.succeeded, ["~vault"]);
}

/// Test that the debug output of the installer and of the setup env has
/// the setup env keys but not the values. Debug output can go to logs.
///   1. Resolve the pack `envy@1` that sets a secret setup env value
///   2. Check that the installer env has the value
///   3. Check that the debug output of the installer and the setup env
///      has the key and not the value
#[test]
fn debug_output_of_installer_and_setup_env_has_no_values() {
    let packs = envy_packs(&format!("setup.env.TOKEN = \"{SECRET}\"\n"), true);
    let resolved = resolve_with(&packs, "[packs]\nenvy = { version = 1 }\n").unwrap();
    let pack = &resolved.packs[0];
    let installer = pack.setup_installer().unwrap();
    assert!(
        installer
            .env
            .contains(&("AIRLOCK_PACK_ENV_TOKEN".to_string(), SECRET.to_string()))
    );
    for text in [format!("{installer:?}"), format!("{:?}", pack.setup_env())] {
        assert!(text.contains("TOKEN"), "{text}");
        assert!(!text.contains(SECRET), "{text}");
    }
}

/// Test that `setup` values that are not valid are refused with a clear
/// error, and that the error does not show the value. The limits come
/// from the exec in the guest. A pack with no setup script cannot use the
/// values, so a setup env there is an error.
///   1. Resolve the pack `envy@1` with each bad `config.lua`
///   2. Check the error text, and that it does not have the secret value
///   3. Check that a pack with no setup script and a setup env is refused
#[test]
fn bad_setup_env_in_config_lua_is_refused_without_showing_value() {
    let cases = [
        (
            format!("setup.env['bad-key'] = '{SECRET}'"),
            "invalid key `setup.env.bad-key`",
        ),
        (
            "setup.env.N = 5".to_string(),
            "invalid value at `setup.env.N`, must be a string",
        ),
        (
            format!("setup.env.T = '{SECRET}\\0'"),
            "invalid value at `setup.env.T`, must be a string without NUL bytes",
        ),
        (
            format!("setup.env.T = '{SECRET}\\xff'"),
            "invalid value at `setup.env.T`, must be a string without NUL bytes",
        ),
        (
            format!("setup.env.T = '{SECRET}' .. string.rep('s', 131072)"),
            "`setup.env.T` is too large: the variable can have 131072 bytes at most",
        ),
        (
            format!(
                "for i = 1, 9 do setup.env['K' .. i] = '{SECRET}' .. string.rep('s', 120000) end"
            ),
            "all variables can have 1048576 bytes at most",
        ),
        (
            format!("for i = 1, 65 do setup.env['K' .. i] = '{SECRET}' end"),
            "`setup.env` has too many keys: 64 is the maximum",
        ),
        (
            format!("setup = '{SECRET}'"),
            "invalid value at `setup`, must be a table",
        ),
        (
            format!("setup.env = '{SECRET}'"),
            "invalid value at `setup.env`, must be a table",
        ),
        ("setup.envs = {}".to_string(), "invalid key `setup.envs`"),
    ];
    for (lua, expected) in cases {
        let error = envy_error(&lua, true);
        assert!(error.contains("pack envy: "), "{lua}: {error}");
        assert!(error.contains(expected), "{lua}: {error}");
        assert!(!error.contains(SECRET), "{lua}: {error}");
    }

    let error = envy_error(&format!("setup.env.TOKEN = '{SECRET}'"), false);
    assert!(
        error.contains("pack envy: config.lua sets `setup.env`, but the pack has no setup.sh"),
        "{error}"
    );
    // An empty setup env is valid with no setup script.
    let packs = envy_packs("setup.env = {}\n", false);
    assert!(resolve_with(&packs, "[packs]\nenvy = { version = 1 }\n").is_ok());
}

/// Test that the fingerprint of a user pack follows the pack files and the
/// setup env values. A changed file or value must install the pack again.
/// The same files and values must not.
///   1. Resolve the user pack `vault` two times in one data directory and
///      check that the fingerprint stays the same
///   2. Change the token file (the files stay the same) and check that the
///      fingerprint changes. Change it back and check the first value.
///   3. Change the setup script and check that the fingerprint changes
#[test]
fn user_pack_fingerprint_follows_files_and_setup_env_values() {
    let tmp = temp_dir();
    let data = data_dir_at(&tmp.path().join("data"));
    let token = tmp.path().join("token.txt");
    std::fs::write(&token, "one").unwrap();
    let dir = tmp.path().join("packs");
    write_user_pack(
        &dir,
        "vault",
        &[
            ("pack.toml", PACK_TOML),
            ("config.lua", &token_config(&token)),
            ("setup.sh", VAULT_SETUP),
        ],
    );
    let fingerprint = || {
        let (packs, _) = with_user_packs(&[&dir], &[]).unwrap();
        let resolved = resolve_in(&data, &packs, "[packs]\n\"~vault\" = {}\n").unwrap();
        resolved.packs[0].setup_installer().unwrap().fingerprint
    };

    let first = fingerprint();
    assert_eq!(fingerprint(), first);
    std::fs::write(&token, "two").unwrap();
    assert_ne!(fingerprint(), first);
    std::fs::write(&token, "one").unwrap();
    assert_eq!(fingerprint(), first);

    std::fs::write(dir.join("vault/setup.sh"), format!("{VAULT_SETUP}true\n")).unwrap();
    assert_ne!(fingerprint(), first);
}

/// Test the fingerprint input of a built-in pack. With no setup env, the
/// input is the same as before the setup env existed, so existing
/// sandboxes keep their installs. With a setup env, the keyed hash of the
/// env goes in.
///   1. Resolve the pack `envy@1` with no setup env and check the
///      fingerprint against the old input `[id, version, args]`
///   2. Resolve it with a setup env and check the fingerprint against the
///      input `[id, version, args, env hash]` with the key of the data
///      directory
#[test]
fn builtin_pack_fingerprint_adds_setup_env_hash_only_when_env_is_set() {
    let tmp = temp_dir();
    let data = data_dir_at(tmp.path());
    let toml = "[packs]\nenvy = { version = 1 }\n";
    let sha = |input: serde_json::Value| hex::encode(Sha256::digest(input.to_string()));

    let resolved = resolve_in(&data, &envy_packs("config.env = {}\n", true), toml).unwrap();
    assert_eq!(
        resolved.packs[0].setup_installer().unwrap().fingerprint,
        sha(serde_json::json!(["envy", "1", {}]))
    );

    let lua = format!("setup.env.TOKEN = '{SECRET}'\n");
    let resolved = resolve_in(&data, &envy_packs(&lua, true), toml).unwrap();
    let key = EnvMacKey::load_or_create(&data).unwrap();
    let mac = hex::encode(SetupEnv::for_test(&[("TOKEN", SECRET)]).mac(&key));
    assert_eq!(
        resolved.packs[0].setup_installer().unwrap().fingerprint,
        sha(serde_json::json!(["envy", "1", {}, mac]))
    );
}
