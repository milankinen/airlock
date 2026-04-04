mod connect_request;
mod http_request;

pub use connect_request::host_matches;
use mlua::{Function, Lua, Value};
use tracing::{debug, trace};

use crate::config::config;

pub struct ScriptEngine {
    http_rules: Vec<CompiledRule>,
    allowed_hosts: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct TcpConnect {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}

#[derive(Debug, Clone)]
pub struct HttpRequestInfo {
    pub connect: TcpConnect,
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub allowed: bool,
    pub denied: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    #[error("request was denied by the rules")]
    Denied,
    #[error("script error: {0}")]
    Lua(#[from] mlua::Error),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

impl ScriptEngine {
    pub fn init(config: &config::Network) -> anyhow::Result<Self> {
        let mut http_rules = Vec::new();

        for rule in &config.rules {
            http_rules.push(compile_rule(rule)?);
        }

        debug!(
            "script engine: {} http rules, {} allowed hosts",
            http_rules.len(),
            config.allowed_hosts.len(),
        );
        Ok(Self {
            http_rules,
            allowed_hosts: config.allowed_hosts.clone(),
        })
    }

    pub fn has_http_rules(&self) -> bool {
        !self.http_rules.is_empty()
    }

    /// Check if a host is allowed by the allowed_hosts patterns.
    pub fn is_host_allowed(&self, host: &str) -> bool {
        self.allowed_hosts.iter().any(|p| host_matches(host, p))
    }

    pub fn intercept_http_request(&self, info: &mut HttpRequestInfo) -> Result<(), ScriptError> {
        for rule in &self.http_rules {
            trace!("running http rule '{}'", rule.name);
            rule.lua.globals().set("req", info.clone())?;
            rule.func
                .call::<()>(())
                .map_err(|e| anyhow::anyhow!("rule '{}': {e}", rule.name))?;
            *info = rule.lua.globals().get("req")?;
            if info.denied {
                debug!("http rule '{}' denied request", rule.name);
                return Err(ScriptError::Denied);
            }
        }
        // Allowed unless explicitly denied by a script
        Ok(())
    }
}

struct CompiledRule {
    name: String,
    lua: Lua,
    func: Function,
}

fn compile_rule(rule: &config::NetworkRule) -> anyhow::Result<CompiledRule> {
    let lua = Lua::new();
    sandbox(&lua)?;

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

    let log_fn = lua.create_function(|_, msg: String| {
        tracing::debug!(target: "script", "{msg}");
        Ok(())
    })?;
    lua.globals().set("log", log_fn)?;

    let func = lua
        .load(&rule.script)
        .set_name(&rule.name)
        .into_function()
        .map_err(|e| anyhow::anyhow!("failed to compile rule '{}': {e}", rule.name))?;

    Ok(CompiledRule {
        name: rule.name.clone(),
        lua,
        func,
    })
}

fn sandbox(lua: &Lua) -> mlua::Result<()> {
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
