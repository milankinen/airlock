mod connect_request;

use mlua::{Function, Lua, Value};
use crate::config::config;
use self::connect_request::ConnectRequest;

pub struct ScriptEngine {
    rules: Vec<CompiledRule>,
    default_mode: config::NetworkMode,
}

#[derive(Debug, Clone)]
pub struct TcpConnect {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}


#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("request was denied by the rules")]
    Denied,
    #[error("script error: {0}")]
    Lua(#[from] mlua::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error)
}



impl ScriptEngine {
    pub fn init(config: &config::Network) -> anyhow::Result<Self> {
        let default_mode = config.default_mode;
        let mut compiled = Vec::new();

        for rule in &config.rules {
            if rule.r#type != config::NetworkRuleType::TcpConnect {
                continue;
            }

            let lua = Lua::new();
            sandbox(&lua)?;

            // Validate and snapshot env vars
            let env_table = lua.create_table()?;
            for (var_name, description) in &rule.env {
                let val = std::env::var(var_name).map_err(|_| {
                    anyhow::anyhow!(
                        "rule '{}': missing required env var `{var_name}` ({description})",
                        rule.name
                    )
                })?;
                env_table.set(var_name.as_str(), val)?;
            }
            lua.globals().set("env", env_table)?;

            // Register log function
            let log_fn = lua.create_function(|_, msg: String| {
                tracing::debug!(target: "script", "{msg}");
                Ok(())
            })?;
            lua.globals().set("log", log_fn)?;

            // Compile script once (syntax check + bytecode)
            let func = lua.load(&rule.script).set_name(&rule.name).into_function().map_err(|e| {
                anyhow::anyhow!("failed to compile rule '{}': {e}", rule.name)
            })?;

            compiled.push(CompiledRule {
                name: rule.name.clone(),
                lua,
                func,
            });
        }

        Ok(Self {
            rules: compiled,
            default_mode,
        })
    }

    pub fn intercept_tcp_connect(&self, connect: TcpConnect) -> Result<TcpConnect, ScriptError> {
        let mut req = ConnectRequest {
            connect,
            allowed: self.default_mode == config::NetworkMode::Allow,
            denied: false,
        };
        for rule in &self.rules {
            rule.lua.globals().set("req", req)?;

            rule.func.call::<()>(())
                .map_err(|e| anyhow::anyhow!("rule '{}': {e}", rule.name))?;

            req = rule.lua.globals().get("req")?;
            if req.denied {
                return Err(ScriptError::Denied);
            }
        }
        if !req.allowed {
            return Err(ScriptError::Denied);
        }
        Ok(req.connect)
    }
}

struct CompiledRule {
    name: String,
    lua: Lua,
    func: Function,
}

fn sandbox(lua: &Lua) -> mlua::Result<()> {
    let globals = lua.globals();
    for name in &["os", "io", "debug", "loadfile", "dofile", "load", "require"] {
        globals.set(*name, Value::Nil)?;
    }

    let _ = lua.set_hook(
        mlua::HookTriggers::new().every_nth_instruction(1_000_000),
        |_lua, _debug| {
            Err(mlua::Error::runtime("script exceeded instruction limit"))
        },
    );

    Ok(())
}
