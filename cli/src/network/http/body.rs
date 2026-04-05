use bytes::Bytes;
use mlua::{FromLua, Lua, LuaSerdeExt, UserData, UserDataMethods, Value};

/// HTTP body userdata — wraps raw bytes with text/json accessors.
#[derive(Clone)]
pub struct Body(pub Bytes);

impl Body {
    pub fn empty() -> Self {
        Self(Bytes::new())
    }
}

impl UserData for Body {
    fn add_methods<M: UserDataMethods<Self>>(methods: &mut M) {
        // text() — return raw bytes as a Lua string
        methods.add_method("text", |_, this, ()| {
            Ok(mlua::BString::from(this.0.to_vec()))
        });

        // json() — parse bytes as JSON, return Lua table
        methods.add_method("json", |lua, this, ()| {
            let json: serde_json::Value = serde_json::from_slice(&this.0)
                .map_err(|e| mlua::Error::runtime(format!("invalid JSON: {e}")))?;
            lua.to_value(&json)
        });

        // len() — byte length
        methods.add_method("len", |_, this, ()| Ok(this.0.len()));

        // tostring metamethod
        methods.add_meta_method(mlua::MetaMethod::ToString, |_, this, ()| {
            Ok(format!("Body({} bytes)", this.0.len()))
        });

        // len metamethod (#body)
        methods.add_meta_method(mlua::MetaMethod::Len, |_, this, ()| Ok(this.0.len()));
    }
}

/// Coerce Lua values to Body:
/// - String → raw bytes
/// - Table → JSON encoded
/// - Body userdata → clone
/// - nil → empty
impl FromLua for Body {
    fn from_lua(value: Value, lua: &Lua) -> mlua::Result<Self> {
        match value {
            Value::UserData(ud) => {
                let body = ud.borrow::<Self>()?;
                Ok(body.clone())
            }
            Value::String(s) => Ok(Body(Bytes::copy_from_slice(&s.as_bytes()))),
            Value::Table(_) => {
                let json: serde_json::Value = lua.from_value(value)?;
                let encoded = serde_json::to_vec(&json)
                    .map_err(|e| mlua::Error::runtime(format!("JSON encode: {e}")))?;
                Ok(Body(Bytes::from(encoded)))
            }
            Value::Nil => Ok(Body::empty()),
            _ => Err(mlua::Error::runtime(
                "expected string, table, Body, or nil for body",
            )),
        }
    }
}
