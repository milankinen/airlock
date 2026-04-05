use std::cell::RefCell;
use std::rc::Rc;

use mlua::{Lua, Value};
use tracing::debug;

use super::{http, matchers};
use crate::config::config;

/// Log sink for middleware scripts. Production uses tracing, tests can collect.
pub type LogFn = Rc<dyn Fn(&str)>;

/// Creates the default log sink that writes to tracing.
pub fn tracing_log() -> LogFn {
    Rc::new(|msg| tracing::debug!(target: "ez::script", "{msg}"))
}

/// Collects log messages for testing.
#[derive(Clone)]
pub struct RequestLog(Rc<RefCell<Vec<String>>>);

impl RequestLog {
    pub fn new() -> (Self, LogFn) {
        let log = Self(Rc::new(RefCell::new(Vec::new())));
        let inner = log.0.clone();
        let log_fn: LogFn = Rc::new(move |msg: &str| inner.borrow_mut().push(msg.to_string()));
        (log, log_fn)
    }

    pub fn messages(&self) -> Vec<String> {
        self.0.borrow().clone()
    }
}

pub struct Middleware {
    http: Vec<http::middleware::CompiledMiddleware>,
    allowed_hosts: Vec<String>,
}

impl Middleware {
    pub fn init(config: &config::Network) -> anyhow::Result<Self> {
        Self::init_with_log(config, &tracing_log())
    }

    pub fn init_with_log(config: &config::Network, log: &LogFn) -> anyhow::Result<Self> {
        let mut http_rules = Vec::new();

        for rule in &config.middleware {
            http_rules.push(http::middleware::compile(rule, log.clone())?);
        }

        debug!(
            "script engine: {} http rules, {} allowed hosts",
            http_rules.len(),
            config.allowed_hosts.len(),
        );
        Ok(Self {
            http: http_rules,
            allowed_hosts: config.allowed_hosts.clone(),
        })
    }

    /// Check if a host is allowed by the allowed_hosts patterns.
    pub fn is_host_allowed(&self, host: &str) -> bool {
        self.allowed_hosts
            .iter()
            .any(|p| matchers::host_matches(host, p))
    }

    pub fn http(&self) -> &[http::middleware::CompiledMiddleware] {
        &self.http
    }
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
