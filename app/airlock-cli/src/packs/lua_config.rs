//! Pack `config.lua` support.
//!
//! Some packs make their config values with a Lua script instead of a static
//! config file. The script runs on the host and can read the pack args and
//! the host, for example the git user name. Pack scripts get a small Lua
//! API.

use std::collections::BTreeMap;

use mlua::{Lua, LuaSerdeExt};

use crate::packs::setup_env::SetupEnv;
use crate::packs::{ArgValue, PackMetadata};
use crate::util::VerifiedPath;

/// `setup.env` values by key (not checked yet), or an error text if
/// `setup` is not valid.
type SetupVars = Result<BTreeMap<String, String>, String>;

/// Error that the Lua function `fail(message)` raises.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Fail(String);

/// Run the `config.lua` script of a pack.
///
/// The script runs on the host each time the config resolves. Thus each
/// `airlock start` and `airlock info` reads the host again.
///
/// The script has the full Lua standard library, thus it can read the
/// host (for example `io.popen("git config get user.name")`).
///
/// The script gets these globals:
///  - `config`: Empty table. The script fills or replaces it with config
///    values, in the same form as a config file.
///  - `setup.env`: Empty table. The script sets string values in it, for
///    example `setup.env.TOKEN = "…"`. The setup script gets each value as
///    `AIRLOCK_PACK_ENV_<KEY>`. The values can be secrets (see
///    [`SetupEnv`]).
///  - `pack.name`: The pack id as a string.
///  - `pack.version`: String, or nil for a pack with no versions.
///  - `pack.args`: Arg values of the entry, defaults included. A bool arg
///    is a boolean, a choice arg is a string.
///  - `pack.directory`: Absolute host path of the pack directory. The
///    pack decides what to keep there. The agent packs keep the host side
///    of their mounts in it.
///  - `fail(message)`: Stops with the config error
///    `pack <name>: <message>`.
///
/// Args:
///  - `pack`: Pack that owns the script
///  - `source`: Source of `config.lua`
///  - `args`: Arg values of the pack entry, defaults included
///  - `directory`: Host directory of the pack (see
///    [`crate::cache::pack_mounts_dir`]). It must exist.
///
/// Returns:
///   The `config` table as a JSON object (an empty table is `{}`), and the
///   setup env. An error `pack <name>: …` for a `fail` call, a Lua error, a
///   `config` that is not a table, a `config` that sets `packs` or
///   `presets`, or a `setup.env` that is not valid. A `setup.env` with keys
///   is not valid for a pack with no setup script.
pub fn evaluate(
    pack: &PackMetadata,
    source: &str,
    args: &BTreeMap<String, ArgValue>,
    directory: &VerifiedPath,
) -> anyhow::Result<(serde_json::Value, SetupEnv)> {
    let name = &pack.id;
    let (value, setup) =
        run(pack, source, args, directory).map_err(|e| match e.downcast_ref::<Fail>() {
            Some(fail) => anyhow::anyhow!(crate::messages::packs::pack_failed(name, fail)),
            None => anyhow::anyhow!(crate::messages::packs::pack_failed(name, e)),
        })?;
    anyhow::ensure!(
        value.is_object(),
        crate::messages::packs::lua_config_not_table(name, &value)
    );
    anyhow::ensure!(
        value.get("packs").is_none() && value.get("presets").is_none(),
        crate::messages::packs::lua_config_reserved_keys(name)
    );
    let setup_env = setup
        .and_then(SetupEnv::new)
        .map_err(|e| anyhow::anyhow!(crate::messages::packs::pack_failed(name, e)))?;
    anyhow::ensure!(
        setup_env.is_empty() || pack.has_setup,
        crate::messages::packs::pack_failed(name, crate::messages::packs::SETUP_ENV_WITHOUT_SETUP)
    );
    Ok((value, setup_env))
}

/// Set the globals (see [`evaluate`]), run `source` and convert `config`
/// to JSON.
/// Returns:
///   The `config` value, and the `setup.env` values or an error text if
///   `setup` is not valid. A Lua error if the script fails.
fn run(
    pack: &PackMetadata,
    source: &str,
    args: &BTreeMap<String, ArgValue>,
    directory: &VerifiedPath,
) -> mlua::Result<(serde_json::Value, SetupVars)> {
    // Pack code is trusted: a built-in pack ships with airlock, and the
    // user approved the files of a user pack or a project pack (see
    // `packs::approval`). Thus the state has the full standard library and
    // no limits, unlike the sandboxed state of network middleware
    // (`network/middleware.rs`).
    let lua = Lua::new();
    let globals = lua.globals();
    globals.set("config", lua.create_table()?)?;
    let setup_table = lua.create_table()?;
    setup_table.set("env", lua.create_table()?)?;
    globals.set("setup", setup_table)?;
    let arg_table = lua.create_table()?;
    for (key, value) in args {
        match value {
            ArgValue::Bool(b) => arg_table.set(key.as_str(), *b)?,
            ArgValue::Text(text) => arg_table.set(key.as_str(), text.as_str())?,
        }
    }
    let pack_table = lua.create_table()?;
    pack_table.set("name", pack.id.as_str())?;
    if let Some(version) = &pack.version {
        pack_table.set("version", version.as_str())?;
    }
    pack_table.set("args", arg_table)?;
    let directory = directory.to_path().map_err(mlua::Error::external)?;
    pack_table.set(
        "directory",
        lua.create_string(directory.as_os_str().as_encoded_bytes())?,
    )?;
    globals.set("pack", pack_table)?;
    let fail = lua.create_function(|_, message: String| -> mlua::Result<()> {
        Err(mlua::Error::external(Fail(message)))
    })?;
    globals.set("fail", fail)?;

    let chunk = match &pack.version {
        Some(version) => format!("@packs/{}@{version}/config.lua", pack.id),
        None => format!("@packs/{}/config.lua", pack.id),
    };
    lua.load(source).set_name(chunk).exec()?;
    let config: mlua::Value = globals.get("config")?;
    let setup = setup_vars(globals.get("setup")?)?;
    Ok((lua.from_value(config)?, setup))
}

/// Read the `setup` global after the script ran.
/// Returns:
///   The `setup.env` values by key (not checked yet), or an error text if
///   `setup` or `setup.env` is not a table, `setup` has an other key, or a
///   value is not UTF-8 text. A Lua error if the table read fails.
fn setup_vars(setup: mlua::Value) -> mlua::Result<SetupVars> {
    use crate::messages::packs as msg;
    // The keys come from the script. Escape them for the terminal.
    let shown = |key: &str| crate::cli::prompt::style::escape_controls(key, false).into_owned();
    let setup = match setup {
        mlua::Value::Nil => return Ok(Ok(BTreeMap::new())),
        mlua::Value::Table(table) => table,
        _ => return Ok(Err(msg::setup_not_table("setup"))),
    };
    let mut env = None;
    for pair in setup.pairs::<mlua::Value, mlua::Value>() {
        let (key, value) = pair?;
        if !matches!(&key, mlua::Value::String(text) if text.as_bytes() == b"env") {
            let key = shown(&key.to_string()?);
            return Ok(Err(msg::setup_key_invalid(format!("setup.{key}"))));
        }
        env = Some(value);
    }
    let env = match env {
        None | Some(mlua::Value::Nil) => return Ok(Ok(BTreeMap::new())),
        Some(mlua::Value::Table(table)) => table,
        Some(_) => return Ok(Err(msg::setup_not_table("setup.env"))),
    };
    let mut vars = BTreeMap::new();
    for pair in env.pairs::<mlua::Value, mlua::Value>() {
        let (key, value) = pair?;
        // The key check comes later. A key that is not a string or not
        // UTF-8 is not valid in any case.
        let key = match &key {
            mlua::Value::String(text) => String::from_utf8_lossy(&text.as_bytes()).into_owned(),
            other => {
                let key = shown(&other.to_string()?);
                return Ok(Err(msg::setup_key_invalid(format!("setup.env.{key}"))));
            }
        };
        // Do not show the value in the errors. It can be a secret.
        let mlua::Value::String(text) = value else {
            return Ok(Err(msg::setup_env_value_not_string(shown(&key))));
        };
        let Ok(text) = String::from_utf8(text.as_bytes().to_vec()) else {
            return Ok(Err(msg::setup_env_value_not_text(shown(&key))));
        };
        vars.insert(key, text);
    }
    Ok(Ok(vars))
}
