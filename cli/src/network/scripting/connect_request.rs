use crate::network::scripting::TcpConnect;
use mlua::{FromLua, Lua, UserData, UserDataFields, UserDataMethods, Value};

#[derive(Debug, Clone)]
pub(super) struct ConnectRequest {
    pub connect: TcpConnect,
    pub allowed: bool,
    pub denied: bool,
}

impl FromLua for ConnectRequest {
    fn from_lua(value: Value, _lua: &Lua) -> mlua::Result<Self> {
        match value {
            Value::UserData(ud) => Ok(ud.borrow::<Self>()?.clone()),
            _ => Err(mlua::Error::runtime("expected ConnectRequest")),
        }
    }
}

impl UserData for ConnectRequest {
    fn add_fields<F: UserDataFields<Self>>(fields: &mut F) {
        fields.add_field_method_get("host", |_, this| Ok(this.connect.host.clone()));
        fields.add_field_method_set("host", |_, this, val: String| {
            this.connect.host = val;
            Ok(())
        });
        fields.add_field_method_get("port", |_, this| Ok(this.connect.port));
        fields.add_field_method_set("port", |_, this, val: u16| {
            this.connect.port = val;
            Ok(())
        });
        fields.add_field_method_get("tls", |_, this| Ok(this.connect.tls));
    }

    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        methods.add_method_mut("allow", |_, this, ()| {
            this.allowed = true;
            Ok(())
        });
        methods.add_method_mut("deny", |_, this, ()| {
            this.denied = true;
            Ok(())
        });
        methods.add_method("hostMatches", |_, this, pattern: String| {
            Ok(host_matches(&this.connect.host, &pattern))
        });
    }
}

fn host_matches(host: &str, pattern: &str) -> bool {
    if pattern.starts_with("*.") {
        let suffix = &pattern[1..]; // ".example.com"
        host.ends_with(suffix) || host == &pattern[2..]
    } else {
        host == pattern
    }
}
