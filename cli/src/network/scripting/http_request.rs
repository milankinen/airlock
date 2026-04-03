use mlua::{FromLua, Lua, UserData, UserDataFields, UserDataMethods, Value};

use super::HttpRequestInfo;
use super::connect_request::host_matches;

impl FromLua for HttpRequestInfo {
    fn from_lua(value: Value, _lua: &Lua) -> mlua::Result<Self> {
        match value {
            Value::UserData(ud) => Ok(ud.borrow::<Self>()?.clone()),
            _ => Err(mlua::Error::runtime("expected HttpRequest")),
        }
    }
}

impl UserData for HttpRequestInfo {
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
        fields.add_field_method_get("method", |_, this| Ok(this.method.clone()));
        fields.add_field_method_get("path", |_, this| Ok(this.path.clone()));
        fields.add_field_method_set("path", |_, this, val: String| {
            this.path = val;
            Ok(())
        });
        // Headers as a Lua table
        fields.add_field_method_get("headers", |lua, this| {
            let table = lua.create_table()?;
            for (k, v) in &this.headers {
                table.set(k.as_str(), v.as_str())?;
            }
            Ok(table)
        });
        fields.add_field_method_set("headers", |_, this, table: mlua::Table| {
            let mut headers = Vec::new();
            for pair in table.pairs::<String, String>() {
                let (k, v) = pair?;
                headers.push((k, v));
            }
            this.headers = headers;
            Ok(())
        });
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
