use mlua::{Lua, Value};

/// Log sink for middleware scripts. Production uses tracing, tests can collect.
pub type LogFn = std::rc::Rc<dyn Fn(&str)>;

/// Creates the default log sink that writes to tracing.
pub fn tracing_log() -> LogFn {
    std::rc::Rc::new(|msg| tracing::debug!(target: "ez::script", "{msg}"))
}

pub(super) fn sandbox(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in ["os", "io", "debug", "loadfile", "dofile", "load", "require"] {
        globals.set(name, Value::Nil)?;
    }

    let _ = lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(1_000_000),
        |_lua, _debug| Err(mlua::Error::runtime("script exceeded instruction limit")),
    );

    Ok(())
}
