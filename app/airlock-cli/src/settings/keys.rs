//! `[monitor.keys]` settings — action-name → key(s) map.
//!
//! Each map entry binds a kebab-case action name (matching the
//! `airlock_monitor::keys::SPEC` table) to one or more key strings.
//! Unset actions fall back to the defaults baked into the SPEC table,
//! so partial overrides are fine — listing only the bindings you want
//! to change is enough.

use std::collections::BTreeMap;

use airlock_monitor::keys::{SPEC, action_for, parse_key};
use airlock_monitor::{Action, KeyBindings};
use smart_config::de::WellKnown;
use smart_config::metadata::BasicTypes;

/// One or more key strings bound to a single action. Deserialised
/// from either a TOML string (`back = "q"`) or array
/// (`cancel = ["esc", "x"]`) thanks to a custom `serde::Deserialize`
/// impl over an internal untagged enum.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct KeyList(pub Vec<String>);

impl<'de> serde::Deserialize<'de> for KeyList {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Either {
            One(String),
            Many(Vec<String>),
        }
        match Either::deserialize(d)? {
            Either::One(s) => Ok(KeyList(vec![s])),
            Either::Many(v) => Ok(KeyList(v)),
        }
    }
}

impl WellKnown for KeyList {
    type Deserializer = smart_config::de::Serde<{ BasicTypes::STRING.or(BasicTypes::ARRAY).raw() }>;
    const DE: Self::Deserializer = smart_config::de::Serde;
}

/// Build a runtime [`KeyBindings`] from the user's `[monitor.keys]`
/// map. Unset actions get the default keys from the canonical SPEC;
/// user-provided actions replace the defaults *for that action*.
///
/// All errors (unknown action names, malformed key strings) are
/// collected into a single multi-line message so the user sees every
/// problem at once.
pub fn into_bindings(user: &BTreeMap<String, KeyList>) -> Result<KeyBindings, String> {
    let mut errors: Vec<String> = Vec::new();

    // Start from the canonical defaults so unset actions stay bound.
    let mut by_action: BTreeMap<Action, Vec<String>> = SPEC
        .iter()
        .map(|(_, a, keys)| (*a, keys.iter().map(|s| (*s).to_string()).collect()))
        .collect();

    // Apply user overrides per-action — replacing the default list.
    for (name, list) in user {
        match action_for(name) {
            Some(action) => {
                by_action.insert(action, list.0.clone());
            }
            None => errors.push(format!(
                "monitor.keys.{name}: unknown action (see the manual for the list)"
            )),
        }
    }

    // Validate every key string under whatever action it ends up under.
    let mut bindings = KeyBindings::default();
    for (action, keys) in &by_action {
        for k in keys {
            if let Err(e) = parse_key(k) {
                let name = SPEC
                    .iter()
                    .find_map(|(n, a, _)| (*a == *action).then_some(*n))
                    .unwrap_or("?");
                errors.push(format!("monitor.keys.{name}: {e}"));
            }
        }
        bindings.bind(*action, keys.clone());
    }

    if errors.is_empty() {
        Ok(bindings)
    } else {
        Err(errors.join("\n"))
    }
}
