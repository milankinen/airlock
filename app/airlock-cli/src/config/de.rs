//! Config parsing helpers.
//!
//! Parses config values that are full config sections themselves, for
//! example the entries of a named table, with defaults and validation at
//! each level. Also formats parse errors as readable text.

use std::cell::Cell;
use std::fmt::{Debug, Write};
use std::marker::PhantomData;

use serde::Serialize;
use serde_json::Value;
use smart_config::de::{DeserializeContext, DeserializeParam};
use smart_config::metadata::{BasicTypes, ParamMetadata};
use smart_config::{DescribeConfig, DeserializeConfig, ErrorWithOrigin};

// Nesting depth, for the indent of error messages.
thread_local! {
    static NEST_DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// Indent for the current nesting depth.
fn indent() -> String {
    let depth = NEST_DEPTH.with(std::cell::Cell::get);
    "  ".repeat(depth)
}

/// Deserializer that parses a JSON object value as a full `smart-config`
/// config struct. Defaults and validation work at all nesting levels.
///
/// `smart-config` does not support `BTreeMap<String, T>` where `T` is a
/// config struct. Thus this deserializer parses each value again through
/// the validation of `smart-config`.
#[derive(Debug)]
pub struct Nested<T>(PhantomData<T>);

/// Make a [`Nested`] deserializer for `T`.
pub const fn nested<T>() -> Nested<T> {
    Nested(PhantomData)
}

impl<T> DeserializeParam<T> for Nested<T>
where
    T: DescribeConfig + DeserializeConfig + Serialize + Debug + Send + Sync + 'static,
{
    const EXPECTING: BasicTypes = BasicTypes::OBJECT;

    fn deserialize_param(
        &self,
        ctx: DeserializeContext<'_>,
        param: &'static ParamMetadata,
    ) -> Result<T, ErrorWithOrigin> {
        let Value::Object(value) = smart_config::de::Serde::<{ BasicTypes::OBJECT.raw() }>
            .deserialize_param(ctx, param)?
        else {
            panic!("not a json object");
        };
        let schema = smart_config::ConfigSchema::new(&T::DESCRIPTION, "");
        let source = smart_config::Json::new("nested config", value);
        let repo = smart_config::ConfigRepository::new(&schema).with(source);
        let parser = repo
            .single::<T>()
            .map_err(|e| ErrorWithOrigin::custom(e.to_string()))?;

        NEST_DEPTH.with(|d| d.set(d.get() + 1));
        let result = match parser.parse() {
            Ok(parsed) => Ok(parsed),
            Err(errors) => {
                let e = format_error("invalid configuration", errors);
                Err(ErrorWithOrigin::custom(e))
            }
        };
        NEST_DEPTH.with(|d| d.set(d.get() - 1));
        result
    }

    fn serialize_param(&self, param: &T) -> Value {
        serde_json::to_value(param).unwrap()
    }
}

/// Format `smart-config` parse errors as human-readable text.
/// Args:
///  - `title`: First line of the message
///  - `errors`: Parse errors, one line each, indented for the nesting depth
pub fn format_error(title: impl Into<String>, errors: smart_config::ParseErrors) -> String {
    let ind = indent();
    let mut msg = title.into();
    for e in errors {
        let path = e.path();
        let detail = if matches!(e.category(), smart_config::ParseErrorCategory::MissingField) {
            "missing".to_string()
        } else {
            e.inner().to_string()
        };
        let _ = write!(msg, "\n{ind}* `{path}` {detail}");
    }
    msg
}
