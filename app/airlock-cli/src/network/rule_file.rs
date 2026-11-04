//! Network rules in the project config file.
//!
//! Writes an approved rule change into the project config file in TOML,
//! JSON or YAML. The change sets only the changed values of the rule, or
//! adds the rule. All other text of the file stays the same, with its
//! comments and its order. A list that does not fit on one line gets one
//! item on each line.
//!
//! The sandbox can write the project files. Thus the change does not trust
//! the rule in the file. It keeps the rule tables that the user approved.
//! The rule in the files must be the approved rule before the write, and
//! the approved rule with the change after the write.

use std::collections::BTreeMap;
use std::io::{Read as _, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};

use anyhow::Context as _;
use toml_edit::{Array, DocumentMut, Item, Table, Value};

use crate::config::config_values::{NetworkRule, RuleEnabled};
use crate::messages::grants as msg;
use crate::util::VerifiedPath;

/// Size limit of the project config file.
const FILE_CAP: u64 = 4 * 1024 * 1024;

/// Name stems of the project config files that can have the rule, in the
/// order of the search. The local file overrides the main file, so it
/// comes first.
const STEMS: [&str; 2] = ["airlock.local", "airlock"];

/// Name of the main project config file. A new rule goes there.
const MAIN_FILE: &str = "airlock.toml";

/// Values of a rule to write into the config file. Only the given values
/// change. The other values of the rule table stay.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct RuleWrite {
    /// Name of the rule.
    pub name: String,
    /// New `enabled` value.
    pub enabled: Option<RuleEnabled>,
    /// New description.
    pub description: Option<String>,
    /// New allow patterns.
    pub allow: Option<Vec<String>>,
    /// New deny patterns.
    pub deny: Option<Vec<String>>,
}

/// Make the TOML value of an `enabled` state.
fn enabled_value(enabled: RuleEnabled) -> Value {
    match enabled {
        RuleEnabled::On => Value::from(true),
        RuleEnabled::Off => Value::from(false),
        RuleEnabled::OnDemand => Value::from(RuleEnabled::ON_DEMAND),
    }
}

/// Make a TOML array of strings.
fn list(items: &[String]) -> Value {
    Value::Array(items.iter().map(String::as_str).collect::<Array>())
}

/// Make the key-value pairs of the given values of a rule.
fn write_items(rule: &RuleWrite) -> Vec<(&'static str, Value)> {
    let mut out = Vec::new();
    if let Some(enabled) = rule.enabled {
        out.push(("enabled", enabled_value(enabled)));
    }
    if let Some(description) = &rule.description {
        out.push(("description", Value::from(description.as_str())));
    }
    if let Some(allow) = &rule.allow {
        out.push(("allow", list(allow)));
    }
    if let Some(deny) = &rule.deny {
        out.push(("deny", list(deny)));
    }
    out
}

/// Make the TOML text of a full rule: the `[network.rules.<name>]` table
/// with all of its values, also the default values.
pub(crate) fn rule_toml(name: &str, rule: &NetworkRule) -> String {
    let mut items = vec![
        ("enabled", enabled_value(rule.enabled)),
        ("description", Value::from(rule.description.as_str())),
        ("allow", list(&rule.allow)),
        ("deny", list(&rule.deny)),
        ("passthrough", Value::from(rule.passthrough)),
        ("inject", list(&rule.inject)),
    ];
    for (key, v) in &mut items {
        wrap_long_array(key, v);
    }
    table_toml(name, items)
}

/// Maximum length of a `key = [...]` line. A longer array gets one item on
/// each line.
const MAX_LINE: usize = 80;

/// Put each item of the array `v` on its own line with a 4-space indent
/// and a trailing comma, if the line `key = v` is longer than `MAX_LINE`.
fn wrap_long_array(key: &str, v: &mut Value) {
    let inline = format!("{key} = {}", v.to_string().trim());
    let Some(array) = v.as_array_mut() else {
        return;
    };
    if inline.chars().count() <= MAX_LINE || array.is_empty() {
        return;
    }
    for item in array.iter_mut() {
        item.decor_mut().set_prefix("\n    ");
        item.decor_mut().set_suffix("");
    }
    array.set_trailing("\n");
    array.set_trailing_comma(true);
}

/// Make the TOML text of the table `[network.rules.<name>]` with `items`.
fn table_toml(name: &str, items: Vec<(&str, Value)>) -> String {
    let mut rule = Table::new();
    for (key, v) in items {
        rule.insert(key, Item::Value(v));
    }
    let mut rules = Table::new();
    rules.set_implicit(true);
    rules.insert(name, Item::Table(rule));
    let mut network = Table::new();
    network.set_implicit(true);
    network.insert("rules", Item::Table(rules));
    let mut doc = DocumentMut::new();
    doc.insert("network", Item::Table(network));
    doc.to_string()
}

/// Get the table of the rule `name` in `doc`, if it has one.
fn rule_table_mut<'a>(doc: &'a mut DocumentMut, name: &str) -> Option<&'a mut Item> {
    doc.get_mut("network")?
        .get_mut("rules")?
        .get_mut(name)
        .filter(|item| !item.is_none())
}

/// Put `new` in place of the value `old`. The comments and the spaces
/// around the old value stay.
fn replace_value(old: &mut Value, mut new: Value) {
    *new.decor_mut() = old.decor().clone();
    *old = new;
}

/// Put the given values of a rule into TOML config text. An array that
/// does not fit on one line gets one item on each line.
/// Args:
///  - `text`: Config text in TOML
///  - `rule`: Values to put
///
/// Returns:
///   The new text. If the text has the rule, only the given values of its
///   table change. Else the table is added at the end. Error if the text is
///   not valid TOML.
fn upsert_in_toml(text: &str, rule: &RuleWrite) -> anyhow::Result<String> {
    let mut doc: DocumentMut = text.parse()?;
    let items = write_items(rule);
    let out = match rule_table_mut(&mut doc, &rule.name) {
        Some(Item::Table(table)) => {
            // Keep the header, its comments, the other keys and the place
            // in the file.
            for (key, mut v) in items {
                wrap_long_array(key, &mut v);
                match table.get_mut(key).and_then(Item::as_value_mut) {
                    Some(old) => replace_value(old, v),
                    None => {
                        table.insert(key, Item::Value(v));
                    }
                }
            }
            doc.to_string()
        }
        Some(item) => match item.as_inline_table_mut() {
            Some(table) => {
                for (key, v) in items {
                    match table.get_mut(key) {
                        Some(old) => replace_value(old, v),
                        None => {
                            table.insert(key, v);
                        }
                    }
                }
                doc.to_string()
            }
            None => anyhow::bail!(msg::rule_not_table(&rule.name)),
        },
        None => {
            // Add the text at the end with no change to the text before it.
            let mut out = text.to_string();
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            if !out.is_empty() {
                out.push('\n');
            }
            let mut items = items;
            for (key, v) in &mut items {
                wrap_long_array(key, v);
            }
            out.push_str(&table_toml(&rule.name, items));
            out
        }
    };
    Ok(out)
}

/// Format of a project config file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Format {
    Toml,
    Json,
    Yaml,
}

impl Format {
    /// Get the format of a file from its extension.
    fn of(file_name: &str) -> Option<Self> {
        match file_name.rsplit_once('.')?.1 {
            "toml" => Some(Format::Toml),
            "json" => Some(Format::Json),
            "yaml" | "yml" => Some(Format::Yaml),
            _ => None,
        }
    }

    /// File name with this format, for the parser.
    fn sample_name(self) -> &'static str {
        match self {
            Format::Toml => "airlock.toml",
            Format::Json => "airlock.json",
            Format::Yaml => "airlock.yaml",
        }
    }
}

/// Parse config text of `format` as JSON. Empty text is `null`.
fn parse_text(format: Format, text: &str) -> anyhow::Result<serde_json::Value> {
    if text.trim().is_empty() {
        return Ok(serde_json::Value::Null);
    }
    crate::config::files::parse_file(format.sample_name(), text).context("the config is not valid")
}

/// Get the rule `name` from config text, as JSON.
fn rule_value(format: Format, text: &str, name: &str) -> anyhow::Result<Option<serde_json::Value>> {
    let value = parse_text(format, text)?;
    Ok(value
        .get("network")
        .and_then(|n| n.get("rules"))
        .and_then(|r| r.get(name))
        .filter(|r| !r.is_null())
        .cloned())
}

/// Make the JSON table of a rule after a write: the table `base` with the
/// given values of `rule`.
/// Args:
///  - `base`: Table of the rule before the write. `None` for a new rule
///  - `rule`: Values of the write
///
/// Returns:
///   The table, or error if `base` is not a table.
fn table_after(
    base: Option<&serde_json::Value>,
    rule: &RuleWrite,
) -> anyhow::Result<serde_json::Value> {
    use serde_json::Value as J;
    let mut table = match base {
        None => serde_json::Map::new(),
        Some(J::Object(table)) => table.clone(),
        Some(_) => anyhow::bail!(msg::rule_not_table(&rule.name)),
    };
    let texts = |items: &[String]| J::Array(items.iter().map(|i| J::from(i.as_str())).collect());
    if let Some(enabled) = rule.enabled {
        let value = match enabled {
            RuleEnabled::On => J::Bool(true),
            RuleEnabled::Off => J::Bool(false),
            RuleEnabled::OnDemand => J::from(RuleEnabled::ON_DEMAND),
        };
        table.insert("enabled".into(), value);
    }
    if let Some(description) = &rule.description {
        table.insert("description".into(), J::from(description.as_str()));
    }
    if let Some(allow) = &rule.allow {
        table.insert("allow".into(), texts(allow));
    }
    if let Some(deny) = &rule.deny {
        table.insert("deny".into(), texts(deny));
    }
    Ok(J::Object(table))
}

/// Get the length of the line in `text` that has the byte `offset`, in
/// characters.
fn line_len_at(text: &str, offset: usize) -> usize {
    let start = text[..offset].rfind('\n').map_or(0, |i| i + 1);
    let end = text[offset..].find('\n').map_or(text.len(), |i| offset + i);
    text[start..end].chars().count()
}

/// Put the given values of a rule into JSON config text.
///
/// Only the entry `network.rules.<name>` changes. An array stays on one
/// line if the line fits in `MAX_LINE` characters. Else each item goes on
/// its own line.
/// Returns:
///   The new text, or error if the text is not valid JSON or the rule
///   cannot be put in it.
fn upsert_in_json(text: &str, rule: &RuleWrite) -> anyhow::Result<String> {
    use jsonc_parser::cst::{CstInputValue, CstRootNode};
    let source = if text.trim().is_empty() { "{}\n" } else { text };
    let root = CstRootNode::parse(source, &jsonc_parser::ParseOptions::default())
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    let top = root
        .object_value()
        .context("the config is not a JSON object")?;
    let table = top
        .object_value_or_create("network")
        .and_then(|n| n.object_value_or_create("rules"))
        .and_then(|r| r.object_value_or_create(&rule.name))
        .with_context(|| format!("network.rules.{} is not an object", rule.name))?;
    let scalar = |key: &str, value: CstInputValue| match table.get(key) {
        Some(prop) => prop.set_value(value),
        None => {
            table.append(key, value);
        }
    };
    if let Some(enabled) = rule.enabled {
        scalar(
            "enabled",
            match enabled {
                RuleEnabled::On => CstInputValue::Bool(true),
                RuleEnabled::Off => CstInputValue::Bool(false),
                RuleEnabled::OnDemand => CstInputValue::String(RuleEnabled::ON_DEMAND.into()),
            },
        );
    }
    if let Some(description) = &rule.description {
        scalar("description", CstInputValue::String(description.clone()));
    }
    for (key, items) in [("allow", &rule.allow), ("deny", &rule.deny)] {
        let Some(items) = items else { continue };
        let value =
            CstInputValue::Array(items.iter().cloned().map(CstInputValue::String).collect());
        scalar(key, value);
        let prop = table.get(key).context("the list is missing")?;
        let inline = format!(
            "{}{}: [{}]",
            prop.indent_text().unwrap_or_default(),
            serde_json::Value::from(key),
            items
                .iter()
                .map(|i| serde_json::Value::from(i.as_str()).to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        if inline.chars().count() > MAX_LINE
            && let Some(array) = prop.array_value()
        {
            array.ensure_multiline();
        }
    }
    let mut out = root.to_string();
    if text.trim().is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Render one YAML scalar string.
fn yaml_scalar(text: &str) -> anyhow::Result<String> {
    Ok(serde_yaml::to_string(text)?.trim_end().to_string())
}

/// Put the given values of a rule into YAML config text.
///
/// Only the entry `network.rules.<name>` changes. Comments and the rest of
/// the text stay. A list is a flow list if the line fits in `MAX_LINE`
/// characters. Else it is a block list.
/// Returns:
///   The new text, or error if the text is not valid YAML or the rule
///   cannot be put in it.
fn upsert_in_yaml(text: &str, rule: &RuleWrite) -> anyhow::Result<String> {
    use std::str::FromStr as _;

    use yaml_edit::path::YamlPath as _;
    let doc = yaml_edit::Document::from_str(text).map_err(|e| anyhow::anyhow!("{e}"))?;
    let path = |key: &str| format!("network.rules.{}.{key}", rule.name);
    let set = |key: &str, value: &dyn Fn(&yaml_edit::Document, &str) -> Result<(), String>| {
        value(&doc, &path(key)).map_err(|e| anyhow::anyhow!("network.rules.{}: {e}", rule.name))
    };
    if let Some(enabled) = rule.enabled {
        set("enabled", &|d, p| {
            match enabled {
                RuleEnabled::On => d.try_set_path(p, true),
                RuleEnabled::Off => d.try_set_path(p, false),
                RuleEnabled::OnDemand => d.try_set_path(p, RuleEnabled::ON_DEMAND),
            }
            .map_err(|e| e.to_string())
        })?;
    }
    if let Some(description) = &rule.description {
        set("description", &|d, p| {
            d.try_set_path(p, description.as_str())
                .map_err(|e| e.to_string())
        })?;
    }
    for (key, items) in [("allow", &rule.allow), ("deny", &rule.deny)] {
        let Some(items) = items else { continue };
        let scalars = items
            .iter()
            .map(|i| yaml_scalar(i))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let flow = format!("[{}]", scalars.join(", "));
        let block = scalars
            .iter()
            .map(|s| format!("- {s}\n"))
            .collect::<Vec<_>>()
            .concat();
        let put = |list: &str| -> anyhow::Result<()> {
            let parsed = yaml_edit::Document::from_str(list).map_err(|e| anyhow::anyhow!("{e}"))?;
            let seq = parsed.as_sequence().context("not a list")?;
            set(key, &|d, p| {
                d.try_set_path(p, &seq).map_err(|e| e.to_string())
            })
        };
        put(&flow)?;
        if !scalars.is_empty() {
            let out = doc.to_string();
            let node = doc
                .try_get_path(&path(key))
                .map_err(|e| anyhow::anyhow!("{e}"))?;
            if line_len_at(&out, node.byte_range().start as usize) > MAX_LINE {
                put(&block)?;
            }
        }
    }
    let mut out = doc.to_string();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Ok(out)
}

/// Put the given values of a rule into config text of `format`.
/// Returns:
///   The new text, and the table of the rule in it as JSON. Error if the
///   text is not valid, or if the new text does not have exactly the old
///   table of the rule with the given values.
fn upsert_in(
    format: Format,
    text: &str,
    rule: &RuleWrite,
) -> anyhow::Result<(String, serde_json::Value)> {
    let expected = table_after(rule_value(format, text, &rule.name)?.as_ref(), rule)?;
    let out = match format {
        Format::Toml => upsert_in_toml(text, rule)?,
        Format::Json => upsert_in_json(text, rule)?,
        Format::Yaml => upsert_in_yaml(text, rule)?,
    };
    // All keys and values must match. An edit that drops, adds or changes
    // other keys of the rule is an error.
    if rule_value(format, &out, &rule.name)?.as_ref() != Some(&expected) {
        anyhow::bail!(msg::rule_cannot_be_put(&rule.name));
    }
    Ok((out, expected))
}

/// Find the project config file of each name stem, as config discovery
/// does: the first file with a known extension.
/// Returns:
///   `(file, format, text)` of the local file and of the main file, in
///   this order. A stem with no file is not in the list.
fn project_files(
    project_dir: &VerifiedPath,
) -> anyhow::Result<Vec<(VerifiedPath, Format, String)>> {
    let mut out = Vec::new();
    for stem in STEMS {
        for ext in crate::config::files::EXTENSIONS {
            let name = format!("{stem}.{ext}");
            let file = project_dir.join(&name)?;
            if let Some(text) = read_file(&file)? {
                let format = Format::of(&name).context("unknown config format")?;
                out.push((file, format, text));
                break;
            }
        }
    }
    Ok(out)
}

/// Get the path of a project file, as config discovery gives it.
fn origin(file: &VerifiedPath) -> String {
    file.given().display().to_string()
}

/// Network rule tables of the project files, as the user approved them.
///
/// The sandbox can write the project files. Thus a rule change does not
/// trust the rule in the file. Before the write, the rule in each project
/// file must be the approved rule. After the write, the rule must be the
/// approved rule with the change, with all keys and values.
#[derive(Clone, Debug, Default)]
pub(crate) struct ApprovedRules {
    /// The `network.rules` table of each project file, by file path.
    files: BTreeMap<String, serde_json::Map<String, serde_json::Value>>,
}

impl ApprovedRules {
    /// Get the approved rule tables of a sandbox.
    /// Args:
    ///  - `sandbox_dir`: Sandbox directory with the config of the last run
    ///  - `project_dir`: Project root directory
    ///
    /// Returns:
    ///   The tables of the config of the last run. The start saves that
    ///   config after the config change check, so it is the config that
    ///   the user approved. If the sandbox has no such config, the tables
    ///   of the project files as they are now. Error if the config or a
    ///   file cannot be read.
    pub(crate) fn load(
        sandbox_dir: &VerifiedPath,
        project_dir: &VerifiedPath,
    ) -> anyhow::Result<Self> {
        if let Some(stored) = crate::sandbox::read_run_meta(sandbox_dir).last_run_config {
            let config: crate::config::LayeredConfig = serde_json::from_value(stored)?;
            return Ok(Self {
                files: config.project_rules().into_iter().collect(),
            });
        }
        let mut files = BTreeMap::new();
        for (file, format, text) in project_files(project_dir)? {
            let rules = parse_text(format, &text)?
                .pointer("/network/rules")
                .and_then(serde_json::Value::as_object)
                .cloned()
                .unwrap_or_default();
            files.insert(origin(&file), rules);
        }
        Ok(Self { files })
    }

    /// Get the approved table of the rule `name` in the project file
    /// `origin`, if the file has the rule.
    fn table(&self, origin: &str, name: &str) -> Option<&serde_json::Value> {
        self.files
            .get(origin)?
            .get(name)
            .filter(|table| !table.is_null())
    }

    /// Set the approved table of the rule `name` in the project file
    /// `origin`.
    fn set(&mut self, origin: &str, name: &str, table: serde_json::Value) {
        self.files
            .entry(origin.to_string())
            .or_default()
            .insert(name.to_string(), table);
    }
}

/// The project file that a change of a rule goes to.
struct Target {
    file: VerifiedPath,
    format: Format,
    /// Text of the file. `None` if the file does not exist.
    text: Option<String>,
    /// Table of the rule in the file. It is the approved table.
    table: Option<serde_json::Value>,
}

/// Find the project file that a change of the rule `name` goes to: the
/// local or the main project file that has the rule, else the main project
/// file, else a new `airlock.toml`.
/// Args:
///  - `project_dir`: Project root directory
///  - `name`: Name of the rule
///  - `approved`: Approved rule tables
///
/// Returns:
///   The file, or error if a project file cannot be read, or if the rule
///   in a project file is not the approved rule.
fn target(
    project_dir: &VerifiedPath,
    name: &str,
    approved: &ApprovedRules,
) -> anyhow::Result<Target> {
    let mut files = Vec::new();
    for (file, format, text) in project_files(project_dir)? {
        let table = rule_value(format, &text, name)
            .with_context(|| format!("cannot read {}", file.given().display()))?;
        // The rule in each file counts in the live rule. Thus a change that
        // the user did not approve in any of them stops the write.
        if table.as_ref() != approved.table(&origin(&file), name) {
            anyhow::bail!(msg::rule_not_approved(name, origin(&file)));
        }
        files.push(Target {
            file,
            format,
            text: Some(text),
            table,
        });
    }
    // An approved table in a file that is not in use now (removed, or
    // replaced by a file with another extension) is also a change.
    for (path, rules) in &approved.files {
        let in_use = files.iter().any(|t| origin(&t.file) == *path);
        if !in_use && rules.get(name).is_some_and(|t| !t.is_null()) {
            anyhow::bail!(msg::rule_not_approved(name, path));
        }
    }
    let is_main = |t: &Target| {
        t.file
            .file_name()
            .is_some_and(|n| !n.to_string_lossy().starts_with("airlock.local."))
    };
    let pick = files
        .iter()
        .position(|t| t.table.is_some())
        .or_else(|| files.iter().position(is_main));
    Ok(match pick {
        Some(i) => files.swap_remove(i),
        None => Target {
            file: project_dir.join(MAIN_FILE)?,
            format: Format::Toml,
            text: None,
            table: None,
        },
    })
}

/// Get the allow and deny patterns of the approved rule `name` in the
/// project file that a change of the rule goes to.
/// Args:
///  - `project_dir`: Project root directory
///  - `name`: Name of the rule
///  - `approved`: Approved rule tables
///
/// Returns:
///   `(allow, deny, table)`: the lists and the full table of the rule.
///   Empty lists and no table if no project file has the rule. Error if
///   the rule in a project file is not the approved rule.
pub(crate) fn file_rule(
    project_dir: &VerifiedPath,
    name: &str,
    approved: &ApprovedRules,
) -> anyhow::Result<(Vec<String>, Vec<String>, Option<serde_json::Value>)> {
    let table = target(project_dir, name, approved)?.table;
    let texts = |key: &str| -> Vec<String> {
        table
            .as_ref()
            .and_then(|f| f.get(key))
            .and_then(serde_json::Value::as_array)
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    Ok((texts("allow"), texts("deny"), table))
}

/// Write the given values of a rule into the project config file.
///
/// The file is the local or the main project file that has the rule, else
/// the main project file, else a new `airlock.toml`. The file can be TOML,
/// JSON or YAML. The write replaces the file in one step. On success, the
/// new table becomes the approved table.
/// Args:
///  - `project_dir`: Project root directory
///  - `rule`: Values to write
///  - `approved`: Approved rule tables
///
/// Returns:
///   The path of the changed file and the approved table of the rule in
///   it as JSON. Error if a file is a symlink, is not valid, or cannot be
///   read or written, or if the rule in a file is not the approved rule
///   before or after the write. After the write, an error puts the old
///   text back.
pub(crate) fn write_rule(
    project_dir: &VerifiedPath,
    rule: &RuleWrite,
    approved: &mut ApprovedRules,
) -> anyhow::Result<(String, serde_json::Value)> {
    let target = target(project_dir, &rule.name, approved)?;
    let path = origin(&target.file);
    let old = target.text.as_deref().unwrap_or_default();
    let (out, expected) =
        upsert_in(target.format, old, rule).with_context(|| format!("cannot change {path}"))?;
    write_atomic(&target.file, out.as_bytes()).with_context(|| format!("cannot write {path}"))?;
    // The sandbox can write the file at the same time. Keep the file only
    // if it has exactly the approved rule now.
    let back = read_file(&target.file)
        .ok()
        .flatten()
        .and_then(|text| rule_value(target.format, &text, &rule.name).ok())
        .flatten();
    if back.as_ref() != Some(&expected) {
        let restored = match &target.text {
            Some(text) => write_atomic(&target.file, text.as_bytes()),
            None => target.file.remove_file().map_err(Into::into),
        };
        if let Err(e) = restored {
            tracing::warn!("rule change: cannot restore {path}: {e:#}");
        }
        anyhow::bail!(msg::rule_write_changed(&rule.name, &path));
    }
    approved.set(&path, &rule.name, expected.clone());
    Ok((path, expected))
}

/// Read a project config file. The open does not follow a symlink, also
/// when the sandbox puts the link there after a check.
/// Returns:
///   The text, or `None` if the file does not exist. Error if the file is
///   a symlink, is not a regular file, is too large or cannot be read.
fn read_file(file: &VerifiedPath) -> anyhow::Result<Option<String>> {
    let shown = file.given().display();
    // The write replaces the file. It must not replace a link, for example
    // into a dotfiles repo, and it must not read a file outside the project
    // through a link that the sandbox put there. A FIFO must not block the
    // open.
    let opened = file.open_with(
        std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK),
    );
    let mut opened = match opened {
        Ok(f) => f,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) if e.raw_os_error() == Some(libc::ELOOP) => {
            anyhow::bail!(msg::rule_file_symlink(shown))
        }
        Err(e) => return Err(e.into()),
    };
    if !opened.metadata()?.is_file() {
        anyhow::bail!(msg::rule_file_not_regular(shown));
    }
    let mut text = String::new();
    (&mut opened).take(FILE_CAP + 1).read_to_string(&mut text)?;
    if text.len() as u64 > FILE_CAP {
        anyhow::bail!(msg::rule_file_too_large(shown));
    }
    Ok(Some(text))
}

/// Write `bytes` to `file` through a temporary file in the same directory
/// and a rename. The file keeps its mode. A new file gets mode 0644.
fn write_atomic(file: &VerifiedPath, bytes: &[u8]) -> anyhow::Result<()> {
    let dir = file.parent().context("no parent directory")?;
    let name = file.file_name().context("no file name")?.to_string_lossy();
    let mode = file
        .symlink_metadata()
        .map_or(0o644, |m| m.permissions().mode() & 0o7777);
    // The pid in the name keeps two processes apart.
    let tmp = dir.join(format!(".{name}.{}.tmp", std::process::id()))?;
    let _ = tmp.remove_file();
    let result = (|| -> anyhow::Result<()> {
        let mut out = tmp.open_with(
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW),
        )?;
        out.write_all(bytes)?;
        out.set_permissions(std::fs::Permissions::from_mode(mode))?;
        out.sync_all()?;
        tmp.rename_to(file)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = tmp.remove_file();
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Put `rule` into the TOML text `text` (see [`upsert_in`]).
    fn toml_upsert(text: &str, rule: &RuleWrite) -> anyhow::Result<String> {
        Ok(upsert_in(Format::Toml, text, rule)?.0)
    }

    /// Write `rule` into the project with the rules of the project files as
    /// the approved rules.
    fn write(
        project: &VerifiedPath,
        rule: &RuleWrite,
    ) -> anyhow::Result<(String, serde_json::Value)> {
        let sandbox = project.join("no-sandbox").unwrap();
        let mut approved = ApprovedRules::load(&sandbox, project)?;
        write_rule(project, rule, &mut approved)
    }

    fn spec(name: &str) -> RuleWrite {
        RuleWrite {
            name: name.into(),
            enabled: Some(RuleEnabled::OnDemand),
            description: Some("npm packages".into()),
            allow: Some(vec!["registry.npmjs.org".into(), "*.npmjs.com".into()]),
            deny: Some(vec!["evil.npmjs.com".into()]),
        }
    }

    const CONFIG: &str = "\
# Project config.
[vm]
cpus = 4   # more cpus

[network]
policy = \"deny-by-default\"

# The npm rule.
[network.rules.npm]
allow = [ \"old.example.com\" ]  # old
passthrough = true

[network.rules.other]
allow = [\"other.example.com\"]
# end of file";

    /// Test that a new rule goes to the end of the file, and that the text
    /// before it does not change. The user's comments and order must stay.
    ///   1. Put a new rule into a config with comments and odd spacing
    ///   2. Check that the old text is the start of the new text
    ///   3. Check that the rest is the TOML of the rule
    #[test]
    fn new_rule_is_appended_and_rest_of_file_stays_same() {
        let out = toml_upsert(CONFIG, &spec("pip")).unwrap();
        let rest = out.strip_prefix(CONFIG).unwrap();
        assert_eq!(
            rest,
            "\n\n[network.rules.pip]\n\
             enabled = \"on-demand\"\n\
             description = \"npm packages\"\n\
             allow = [\"registry.npmjs.org\", \"*.npmjs.com\"]\n\
             deny = [\"evil.npmjs.com\"]\n"
        );
        assert_eq!(toml_upsert("", &spec("pip")).unwrap(), rest.trim_start());
    }

    /// Test that a change of an existing rule sets only the given values.
    /// The header with its comment, the other keys of the rule, the comment
    /// of a changed value and all other text stay the same.
    ///   1. Put new values into the rule `npm`, which has `passthrough`
    ///   2. Check that the text before and after the rule is the same
    ///   3. Check the rule table: changed values in place, `passthrough`
    ///      kept, new keys at the end
    ///   4. Change only `enabled` and check that the rest stays
    #[test]
    fn changed_rule_sets_given_values_and_keeps_other_keys() {
        let out = toml_upsert(CONFIG, &spec("npm")).unwrap();
        let head = "# Project config.\n[vm]\ncpus = 4   # more cpus\n\n[network]\n\
                    policy = \"deny-by-default\"\n\n# The npm rule.\n[network.rules.npm]\n";
        let tail = "\n[network.rules.other]\nallow = [\"other.example.com\"]\n# end of file";
        assert!(out.starts_with(head), "{out}");
        assert!(out.ends_with(tail), "{out}");
        assert_eq!(
            &out[head.len()..out.len() - tail.len()],
            "allow = [\"registry.npmjs.org\", \"*.npmjs.com\"]  # old\n\
             passthrough = true\n\
             enabled = \"on-demand\"\n\
             description = \"npm packages\"\n\
             deny = [\"evil.npmjs.com\"]\n"
        );
        let only_enabled = RuleWrite {
            name: "npm".into(),
            enabled: Some(RuleEnabled::On),
            ..RuleWrite::default()
        };
        let again = toml_upsert(&out, &only_enabled).unwrap();
        assert_eq!(
            again,
            out.replace("enabled = \"on-demand\"", "enabled = true")
        );
    }

    /// Test that the TOML text of a rule has all keys, also the default
    /// values, keeps a short array on one line, and puts each item of a
    /// long array on its own line. The prompt must show the full rule, and
    /// a line over 80 characters is hard to read in the prompt.
    ///   1. Make the text of a rule with a short deny list and a long allow
    ///      list
    ///   2. Check the full text
    #[test]
    fn rule_toml_has_all_keys_and_wraps_arrays_longer_than_80_characters() {
        let rule = NetworkRule {
            enabled: RuleEnabled::On,
            description: String::new(),
            allow: (0..4)
                .map(|i| format!("host{i}.registry.example.com"))
                .collect(),
            deny: vec!["evil.example.com".into()],
            passthrough: false,
            inject: vec![],
        };
        assert_eq!(
            rule_toml("npm", &rule),
            "[network.rules.npm]\n\
             enabled = true\n\
             description = \"\"\n\
             allow = [\n    \"host0.registry.example.com\",\n    \"host1.registry.example.com\",\n    \
             \"host2.registry.example.com\",\n    \"host3.registry.example.com\",\n]\n\
             deny = [\"evil.example.com\"]\n\
             passthrough = false\n\
             inject = []\n"
        );
    }

    /// Test that a rule that cannot go into the file is an error, and not a
    /// broken file. An inline `rules` table cannot get a new table header.
    #[test]
    fn rule_that_does_not_fit_file_is_error() {
        let config = "[network]\nrules = { a = { allow = [\"a.com\"] } }\n";
        assert!(toml_upsert(config, &spec("npm")).is_err());
    }

    /// Test that the write goes to the file that has the rule, else to
    /// `airlock.toml`, and that a symlink is refused.
    ///   1. Write a new rule into an empty project dir
    ///   2. Put the rule into `airlock.local.toml` and write it again
    ///   3. Check that the local file changed and the main file did not
    ///   4. Replace the main file with a link and check the error
    #[test]
    fn write_goes_to_file_with_rule_and_refuses_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let project = VerifiedPath::new_follow(dir.path(), None).unwrap();
        let main = project.join("airlock.toml").unwrap();
        let local = project.join("airlock.local.toml").unwrap();

        let (path, table) = write(&project, &spec("npm")).unwrap();
        assert_eq!(table["enabled"], "on-demand");
        assert!(path.ends_with("airlock.toml"), "{path}");
        let main_text = main.read_to_string().unwrap();
        assert!(main_text.contains("[network.rules.npm]"));

        local
            .write("[network.rules.npm]\nenabled = \"on-demand\"\n")
            .unwrap();
        let mut changed = spec("npm");
        changed.allow = Some(vec!["new.example.com".into()]);
        write(&project, &changed).unwrap();
        assert!(local.read_to_string().unwrap().contains("new.example.com"));
        assert_eq!(main.read_to_string().unwrap(), main_text);

        main.remove_file().unwrap();
        local.remove_file().unwrap();
        std::os::unix::fs::symlink(
            dir.path().join("elsewhere"),
            dir.path().join("airlock.toml"),
        )
        .unwrap();
        let err = write(&project, &spec("pip")).unwrap_err().to_string();
        assert!(err.contains("symlink"), "{err}");
    }

    /// Long allow list for the wrap tests.
    fn long_allow() -> Vec<String> {
        (0..4)
            .map(|i| format!("host{i}.registry.example.com"))
            .collect()
    }

    /// Test that a written TOML rule gets a long list with one item on each
    /// line, and that the rest of the file stays the same.
    ///   1. Put a long allow list into the rule `npm` of a config
    ///   2. Check the text before the rule and the wrapped list
    #[test]
    fn written_toml_list_longer_than_80_characters_wraps() {
        let rule = RuleWrite {
            name: "npm".into(),
            allow: Some(long_allow()),
            ..RuleWrite::default()
        };
        let out = toml_upsert(CONFIG, &rule).unwrap();
        let head = &CONFIG[..CONFIG.find("allow = [ ").unwrap()];
        assert!(out.starts_with(head), "{out}");
        assert!(
            out.contains(
                "allow = [\n    \"host0.registry.example.com\",\n    \
                 \"host1.registry.example.com\",\n    \"host2.registry.example.com\",\n    \
                 \"host3.registry.example.com\",\n]  # old\npassthrough = true\n"
            ),
            "{out}"
        );
        assert!(
            out.ends_with("[network.rules.other]\nallow = [\"other.example.com\"]\n# end of file")
        );
    }

    const JSON: &str = r#"{
    "vm": { "cpus": 4 },
    "network": {
        "policy": "deny-by-default",
        "rules": {
            "npm": {
                "allow": ["old.example.com"],
                "passthrough": true
            }
        }
    }
}
"#;

    /// Test that a JSON rule change sets only the given values, keeps the
    /// other keys, the indent and the rest of the file, and wraps a long
    /// list. A new rule goes into the `rules` object.
    ///   1. Change the rule `npm` and check the full text
    ///   2. Add the rule `pip` with a long list and check its text
    ///   3. Add a rule to an empty file
    #[test]
    fn json_rule_change_keeps_rest_of_file_and_wraps_long_list() {
        let out = upsert_in(Format::Json, JSON, &spec("npm")).unwrap().0;
        assert_eq!(
            out,
            JSON.replace(
                r#""allow": ["old.example.com"],
                "passthrough": true"#,
                r#""allow": ["registry.npmjs.org", "*.npmjs.com"],
                "passthrough": true,
                "enabled": "on-demand",
                "description": "npm packages",
                "deny": ["evil.npmjs.com"]"#
            )
        );
        let rule = RuleWrite {
            name: "pip".into(),
            allow: Some(long_allow()),
            ..RuleWrite::default()
        };
        let out = upsert_in(Format::Json, JSON, &rule).unwrap().0;
        assert!(
            out.starts_with(&JSON[..JSON.find("            }\n").unwrap()]),
            "{out}"
        );
        assert!(
            out.contains(
                "            \"pip\": {\n                \"allow\": [\n                    \
                 \"host0.registry.example.com\",\n"
            ),
            "{out}"
        );
        assert!(
            out.contains("\"host3.registry.example.com\"\n                ]\n"),
            "{out}"
        );
        let out = upsert_in(Format::Json, "", &spec("npm")).unwrap().0;
        assert!(out.contains("\"enabled\": \"on-demand\""), "{out}");
    }

    const YAML: &str = "\
# Project config.
vm:
  cpus: 4 # more cpus
network:
  policy: deny-by-default
  rules:
    # The npm rule.
    npm:
      allow: [old.example.com] # old
      passthrough: true
    other:
      allow:
        - other.example.com
# end of file
";

    /// Test that a YAML rule change sets only the given values, keeps the
    /// comments, the other keys and the rest of the file, and makes a block
    /// list from a long list. A new rule goes into the `rules` mapping.
    ///   1. Change the rule `npm` and check the full text
    ///   2. Add the rule `pip` with a long list and check its text
    ///   3. Add a rule to an empty file
    #[test]
    fn yaml_rule_change_keeps_comments_and_wraps_long_list() {
        let out = upsert_in(Format::Yaml, YAML, &spec("npm")).unwrap().0;
        assert_eq!(
            out,
            YAML.replace(
                "      allow: [old.example.com] # old\n      passthrough: true\n",
                "      allow: [registry.npmjs.org, '*.npmjs.com'] # old\n      \
                 passthrough: true\n      enabled: on-demand\n      \
                 description: npm packages\n      deny: [evil.npmjs.com]\n"
            )
        );
        let rule = RuleWrite {
            name: "pip".into(),
            allow: Some(long_allow()),
            ..RuleWrite::default()
        };
        let out = upsert_in(Format::Yaml, YAML, &rule).unwrap().0;
        assert!(
            out.starts_with(&YAML[..YAML.find("# end").unwrap()]),
            "{out}"
        );
        assert!(
            out.contains(
                "    pip:\n      allow:\n        - host0.registry.example.com\n        \
                 - host1.registry.example.com\n"
            ),
            "{out}"
        );
        let out = upsert_in(Format::Yaml, "", &spec("npm")).unwrap().0;
        assert!(out.contains("enabled: on-demand"), "{out}");
    }

    /// Test that the write goes to a JSON or YAML main file when the
    /// project has one, and gives the rule as JSON for the config of the
    /// last run.
    ///   1. Write a rule into a project with `airlock.json`
    ///   2. Write a rule into a project with `airlock.yml`
    ///   3. Check the changed files and the returned rules
    #[test]
    fn write_goes_to_json_or_yaml_main_file() {
        for (name, text) in [
            ("airlock.json", "{}\n"),
            ("airlock.yml", "vm:\n  cpus: 2\n"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join(name), text).unwrap();
            let project = VerifiedPath::new_follow(dir.path(), None).unwrap();
            let (path, table) = write(&project, &spec("npm")).unwrap();
            assert!(path.ends_with(name), "{path}");
            assert_eq!(table["allow"][1], "*.npmjs.com");
            assert!(!dir.path().join("airlock.toml").exists());
            let saved = std::fs::read_to_string(dir.path().join(name)).unwrap();
            assert!(saved.contains("evil.npmjs.com"), "{saved}");
        }
    }
}
