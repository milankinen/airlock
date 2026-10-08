//! Deep merge of config documents ([`merge_json`]), and the check that
//! the config values of the enabled packs do not conflict
//! ([`pack_conflicts`]).

/// Rewrite every plain-string `[env]` entry of one config layer into its
/// object form `{ "value": "..." }` before layers are merged.
///
/// `merge_json` lets a primitive overlay replace an object wholesale, so
/// without this a `TOKEN = "${TOKEN}"` in `airlock.local.toml` would erase a
/// base layer's `{ value = "${TOKEN}", mask = true }` — silently un-masking
/// the secret. With both sides in object form the merge is field-wise: an
/// overlay string only replaces `value` and inherits the base's `mask`.
pub(crate) fn normalize_env(layer: &mut serde_json::Value) {
    let Some(env) = layer
        .get_mut("env")
        .and_then(serde_json::Value::as_object_mut)
    else {
        return;
    };
    for entry in env.values_mut() {
        if entry.is_string() {
            let value = std::mem::take(entry);
            *entry = serde_json::json!({ "value": value });
        }
    }
}

/// Merge two JSON values with custom rules:
/// - Null overlay: base wins (null never overwrites)
/// - Arrays: concatenate
/// - Objects: recursive merge
/// - Primitives: overlay wins
/// - Type mismatch: overlay wins
pub(crate) fn merge_json(base: serde_json::Value, overlay: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match (base, overlay) {
        (base, Value::Null) => base,
        (Value::Object(mut base), Value::Object(overlay)) => {
            for (key, overlay_val) in overlay {
                let merged = match base.remove(&key) {
                    Some(base_val) => merge_json(base_val, overlay_val),
                    None => overlay_val,
                };
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (Value::Array(mut base), Value::Array(overlay)) => {
            base.extend(overlay);
            Value::Array(base)
        }
        (_, overlay) => overlay,
    }
}

/// Find the values that two of the enabled packs set differently: `docs`
/// are their config values, each `(pack name, value)` in pack order and
/// env-normalized (see [`normalize_env`]), so env entries compare per
/// field.
///
/// The values merge like [`merge_json`]: objects key by key, arrays
/// concatenate, and a null is no value (it never overwrites, and a later
/// pack can set the path). A scalar, or a value of another type, at a path
/// that an earlier pack set is a conflict unless it equals the value
/// there. Returns one line per conflict (none: the packs merge the same in
/// any order, but for the order of array items):
/// ``packs nodejs and python both set `env.FOO.value` ("a" vs "b")``.
pub(crate) fn pack_conflicts(docs: &[(String, serde_json::Value)]) -> Vec<String> {
    let mut owners = Owners::new();
    let mut conflicts = Vec::new();
    let mut merged = serde_json::Value::Object(serde_json::Map::new());
    for (pack, doc) in docs {
        merged = merge_pack_value(
            merged,
            doc.clone(),
            &mut vec![],
            pack,
            &mut owners,
            &mut conflicts,
        );
    }
    conflicts
}

/// The pack that put a value at a path (the keys from the top level). A
/// path inside that value belongs to the same pack, unless a later pack
/// added it.
type Owners = std::collections::BTreeMap<Vec<String>, String>;

/// Merge `overlay` of `pack` onto `base` at `path` (see
/// [`pack_conflicts`]): record the paths that `pack` adds in `owners` and
/// the conflicts in `conflicts`.
fn merge_pack_value(
    base: serde_json::Value,
    overlay: serde_json::Value,
    path: &mut Vec<String>,
    pack: &str,
    owners: &mut Owners,
    conflicts: &mut Vec<String>,
) -> serde_json::Value {
    use serde_json::Value;
    match (base, overlay) {
        (base, Value::Null) => base,
        (Value::Object(mut base), Value::Object(overlay)) => {
            for (key, overlay_val) in overlay {
                path.push(key.clone());
                let merged = if let Some(base_val) = base.remove(&key) {
                    merge_pack_value(base_val, overlay_val, path, pack, owners, conflicts)
                } else {
                    if !overlay_val.is_null() {
                        owners.insert(path.clone(), pack.to_string());
                    }
                    overlay_val
                };
                path.pop();
                base.insert(key, merged);
            }
            Value::Object(base)
        }
        (Value::Array(mut base), Value::Array(overlay)) => {
            base.extend(overlay);
            Value::Array(base)
        }
        // A null that an earlier pack set is unset, not a value.
        (Value::Null, overlay) => {
            owners.insert(path.clone(), pack.to_string());
            overlay
        }
        (base, overlay) => {
            if base != overlay {
                let owner = (0..=path.len())
                    .rev()
                    .find_map(|len| owners.get(&path[..len]))
                    .map_or(pack, String::as_str);
                conflicts.push(format!(
                    "packs {owner} and {pack} both set `{}` ({base} vs {overlay})",
                    path.join(".")
                ));
            }
            base
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn packs_setting_same_path_differently_conflict_naming_owner() {
        let docs = [
            (
                "a",
                json!({
                    "vm": { "image": "x", "cpus": null },
                    "mounts": { "m": { "source": "s" } },
                    "rules": { "r": { "allow": ["1"] } },
                }),
            ),
            (
                "b",
                json!({
                    "vm": { "image": "x", "cpus": 2 },
                    "rules": { "r": { "allow": ["2"] } },
                }),
            ),
            (
                "c",
                json!({
                    "vm": { "cpus": 4 },
                    "mounts": { "m": { "source": "t", "target": "u" } },
                }),
            ),
        ]
        .map(|(name, doc)| (name.to_string(), doc));
        assert_eq!(
            pack_conflicts(&docs),
            [
                "packs a and c both set `mounts.m.source` (\"s\" vs \"t\")",
                "packs b and c both set `vm.cpus` (2 vs 4)",
            ]
        );
    }
}
