//! Pack `config.lua` support.
//!
//! Some packs make their config values with a Lua script instead of a static
//! config file. The script runs on the host and can read the pack args and
//! the host, for example the git user name. Pack scripts get a small Lua
//! API.

use std::collections::BTreeMap;
use std::path::Path;

use mlua::{Lua, LuaSerdeExt};

use crate::packs::{ArgValue, PackMetadata};

/// Error that the Lua function `fail(message)` raises.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Fail(String);

/// Run the `config.lua` script of a pack.
///
/// The script runs on the host each time the config resolves. Thus each
/// `airlock start` and `airlock show` reads the host again.
///
/// The script has the full Lua standard library, thus it can read the
/// host (for example `io.popen("git config get user.name")`).
///
/// The script gets these globals:
///  - `config`: Empty table. The script fills or replaces it with config
///    values, in the same form as a config file.
///  - `pack.name`, `pack.version`: Strings.
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
///   The `config` table as a JSON object (an empty table is `{}`). An error
///   `pack <name>: …` for a `fail` call, a Lua error, a `config` that is
///   not a table, or a `config` that sets `packs` or `presets`.
pub fn evaluate(
    pack: &PackMetadata,
    source: &str,
    args: &BTreeMap<String, ArgValue>,
    directory: &Path,
) -> anyhow::Result<serde_json::Value> {
    let name = &pack.name;
    let value = run(pack, source, args, directory).map_err(|e| match e.downcast_ref::<Fail>() {
        Some(fail) => anyhow::anyhow!("pack {name}: {fail}"),
        None => anyhow::anyhow!("pack {name}: {e}"),
    })?;
    anyhow::ensure!(
        value.is_object(),
        "pack {name}: config.lua must leave `config` a table, not {value}"
    );
    anyhow::ensure!(
        value.get("packs").is_none() && value.get("presets").is_none(),
        "pack {name}: config.lua cannot set `packs` or `presets`"
    );
    Ok(value)
}

/// Set the globals (see [`evaluate`]), run `source` and convert `config`
/// to JSON.
fn run(
    pack: &PackMetadata,
    source: &str,
    args: &BTreeMap<String, ArgValue>,
    directory: &Path,
) -> mlua::Result<serde_json::Value> {
    // Pack code is trusted because it ships with airlock. Thus the state
    // has the full standard library and no limits, unlike the sandboxed
    // state of network middleware (`network/middleware.rs`).
    let lua = Lua::new();
    let globals = lua.globals();
    globals.set("config", lua.create_table()?)?;
    let arg_table = lua.create_table()?;
    for (key, value) in args {
        match value {
            ArgValue::Bool(b) => arg_table.set(key.as_str(), *b)?,
            ArgValue::Text(text) => arg_table.set(key.as_str(), text.as_str())?,
        }
    }
    let pack_table = lua.create_table()?;
    pack_table.set("name", pack.name.as_str())?;
    pack_table.set("version", pack.version.as_str())?;
    pack_table.set("args", arg_table)?;
    pack_table.set(
        "directory",
        lua.create_string(directory.as_os_str().as_encoded_bytes())?,
    )?;
    globals.set("pack", pack_table)?;
    let fail = lua.create_function(|_, message: String| -> mlua::Result<()> {
        Err(mlua::Error::external(Fail(message)))
    })?;
    globals.set("fail", fail)?;

    lua.load(source)
        .set_name(format!("@packs/{}@{}/config.lua", pack.name, pack.version))
        .exec()?;
    let config: mlua::Value = globals.get("config")?;
    lua.from_value(config)
}
