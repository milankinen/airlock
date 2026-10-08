//! Monitor key binding settings.
//!
//! Reads the `[monitor.keys]` settings and makes the key bindings of the
//! monitor TUI from them. Each entry binds an action name to one or more
//! keys. Actions that are not set keep their defaults, so the user must list
//! only the bindings to change.

use std::collections::BTreeMap;

use airlock_monitor::keys::{SPEC, action_for, parse_key};
use airlock_monitor::{Action, KeyBindings};
use smart_config::de::WellKnown;
use smart_config::metadata::BasicTypes;

/// One or more key strings bound to one action. Deserializes from a TOML
/// string (`back = "q"`) or array (`cancel = ["esc", "x"]`).
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct KeyList(pub Vec<String>);

impl<'de> serde::Deserialize<'de> for KeyList {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        // An untagged enum accepts both a single string and an array.
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

/// Build the runtime [`KeyBindings`] from the user's `[monitor.keys]` map.
/// Actions that are not set get their default keys from the SPEC table. A
/// user binding replaces the defaults of that action only.
/// Args:
///  - `user`: The user's `[monitor.keys]` map
///
/// Returns:
///   The key bindings, or one multi-line error message with all problems
///   (unknown action names, malformed key strings). Thus the user sees all
///   problems at once.
pub fn into_bindings(user: &BTreeMap<String, KeyList>) -> Result<KeyBindings, String> {
    let mut errors: Vec<String> = Vec::new();

    // Start from the defaults, so actions that are not set stay bound.
    let mut by_action: BTreeMap<Action, Vec<String>> = SPEC
        .iter()
        .map(|(_, a, keys)| (*a, keys.iter().map(|s| (*s).to_string()).collect()))
        .collect();

    // Apply the user bindings. Each one replaces the default list of its
    // action.
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

    // Validate all key strings of all actions.
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
