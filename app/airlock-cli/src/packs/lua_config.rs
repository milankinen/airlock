//! The `config.lua` of a pack: Lua code that makes the pack's config
//! values from its args, or from the host (for example
//! `io.popen("git config get user.name")`). It runs on the host each time
//! the config resolves, so every `airlock start` and `airlock show` reads
//! the host again.
//!
//! Pack code is trusted (it ships with airlock): the Lua state has the
//! standard library and no limits, unlike the sandboxed state of network
//! middleware (`network/middleware.rs`). Its globals:
//!
//! - `config`: an empty table that the code fills (or replaces) with the
//!   config values, as a config file holds them;
//! - `pack`: the pack and its entry:
//!   - `pack.name`, `pack.version`: strings;
//!   - `pack.args`: the arg values of the entry, defaults filled (a bool
//!     arg is a boolean, a choice arg a string);
//!   - `pack.directory`: the absolute host path of the pack's own
//!     directory ([`crate::cache::pack_mounts_dir`]), which exists. The
//!     pack decides what goes there; the agent packs keep the host side
//!     of their mounts in it;
//! - `fail(message)`: stop with the config error `pack <name>: <message>`.

use std::collections::BTreeMap;
use std::path::Path;

use mlua::{Lua, LuaSerdeExt};

use crate::packs::{ArgValue, PackMetadata};

/// The error that `fail(message)` raises.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct Fail(String);

/// Run the `config.lua` `source` of `pack` with `args` and the pack's
/// `directory`, and return its
/// `config`: an object (an empty table is `{}`). A `fail` call, a Lua
/// error and a `config` that is not a table are errors
/// `pack <name>: …`; so is a `config` that sets `packs` or `presets`.
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

/// Set the globals, run `source` and convert `config`.
fn run(
    pack: &PackMetadata,
    source: &str,
    args: &BTreeMap<String, ArgValue>,
    directory: &Path,
) -> mlua::Result<serde_json::Value> {
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
