//! Lua sandbox for HTTP middleware scripts.
//!
//! Restricts what middleware scripts can do, so that user scripts cannot
//! access the host or run forever. Also sends script log messages to the
//! airlock log.

use mlua::{Lua, Value};

/// Log callback for middleware scripts. Production code uses tracing.
/// Tests can collect the messages.
pub type LogFn = std::rc::Rc<dyn Fn(&str)>;

/// Make the default log callback, which writes to tracing.
pub fn tracing_log() -> LogFn {
    std::rc::Rc::new(|msg| tracing::debug!(target: "airlock::script", "{msg}"))
}

/// Remove dangerous globals and set an instruction count limit. Then user
/// scripts cannot escape the sandbox or run forever.
pub(super) fn sandbox(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in ["os", "io", "debug", "loadfile", "dofile", "load", "require"] {
        globals.set(name, Value::Nil)?;
    }

    let _ = lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(100_000),
        |_lua, _debug| Err(mlua::Error::runtime("script exceeded instruction limit")),
    );

    Ok(())
}
