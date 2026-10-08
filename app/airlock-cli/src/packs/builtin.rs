//! The built-in packs: the folders `packs/<name>@<version>/` at the
//! repository root, embedded in the binary, and their loader.
//!
//! A folder holds `pack.toml` (label, description, kind and args), at
//! most one config file `config.{toml,json,yaml,yml,lua}` and the setup
//! script `setup.sh`; only `pack.toml` is required. The packs are ordered
//! by kind (distro, agent, tool), then by name. Names that start with `.`
//! or end with `~` (`.DS_Store`, editor backups) are skipped; any other
//! file is an error.

use std::collections::{BTreeMap, HashSet};
use std::path::Path;
use std::sync::Arc;

use anyhow::{Context, anyhow, bail, ensure};
use include_dir::{Dir, DirEntry, File, include_dir};
use serde::Deserialize;
use serde_json::Value;

use crate::config::files::{EXTENSIONS, parse_file};
use crate::packs::{
    ArgKind, Pack, PackArg, PackConfig, PackKind, PackManager, PackMetadata, PackVersionData,
};

/// The `packs/` directory of the repository.
pub static BUILTIN_PACKS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../packs");

const PACK_FILE: &str = "pack.toml";
const LUA_CONFIG_FILE: &str = "config.lua";
const SETUP_FILE: &str = "setup.sh";

/// The keys of a `[packs]` entry, which no arg can have.
const RESERVED_ARG_KEYS: [&str; 3] = ["version", "enabled", "args"];

/// `pack.toml` of a version folder.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PackFile {
    label: String,
    /// What the version does, in one line (see
    /// [`PackMetadata::description`]); the loader checks that it is not
    /// empty.
    description: String,
    kind: PackKind,
    #[serde(default)]
    args: Vec<ArgEntry>,
}

/// One `[[args]]` entry of `pack.toml`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ArgEntry {
    key: String,
    r#type: ArgType,
    description: String,
    default: Value,
    /// A choice arg: its values.
    values: Option<Vec<String>>,
    /// A choice arg: whether any non-empty string is a value too.
    other: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
enum ArgType {
    Bool,
    Choice,
}

/// Load and validate the packs of `root` (a `packs/` directory).
pub fn load(root: &'static Dir<'static>) -> anyhow::Result<PackManager> {
    // The versions of each pack, by name.
    let mut by_name: BTreeMap<&'static str, Vec<(u64, PackVersionData)>> = BTreeMap::new();
    for entry in root.entries() {
        let file_name = file_name(entry.path())?;
        if is_skipped(file_name) {
            continue;
        }
        let DirEntry::Dir(folder) = entry else {
            bail!("packs/{file_name}: unknown file (a pack is a folder <name>@<version>)");
        };
        let (name, version) = file_name
            .split_once('@')
            .filter(|(name, version)| is_identifier(name) && is_version(version))
            .with_context(|| {
                format!(
                    "packs/{file_name}: the folder name must be <name>@<version> \
                     (name: [a-z][a-z0-9-]*, version: a whole number from 1)"
                )
            })?;
        let number = version.parse::<u64>()?;
        let data = load_version(name, version, folder)?;
        by_name.entry(name).or_default().push((number, data));
    }

    let mut groups: Vec<Vec<PackVersionData>> = Vec::new();
    for (name, mut versions) in by_name {
        // The numbers in ascending order.
        versions.sort_by_key(|(number, _)| *number);
        let first = &versions[0].1.metadata;
        for (_, data) in &versions[1..] {
            let at = format!("packs/{name}@{}/{PACK_FILE}", data.metadata.version);
            ensure!(
                data.metadata.label == first.label,
                "{at}: the label \"{}\" differs from \"{}\" of the other versions",
                data.metadata.label,
                first.label
            );
            ensure!(
                data.metadata.kind == first.kind,
                "{at}: the kind differs from the kind of the other versions"
            );
        }
        groups.push(versions.into_iter().map(|(_, data)| data).collect());
    }
    // Stable: the names stay in order within a kind.
    groups.sort_by_key(|versions| versions[0].metadata.kind);
    Ok(PackManager {
        packs: groups
            .into_iter()
            .flatten()
            .map(|data| Pack(Arc::new(data)))
            .collect(),
    })
}

/// Load the folder `folder` of the pack `name` at version `version`.
fn load_version(
    name: &str,
    version: &str,
    folder: &'static Dir<'static>,
) -> anyhow::Result<PackVersionData> {
    let at = |file_name: &str| format!("packs/{}/{file_name}", folder.path().display());
    let mut files: BTreeMap<&'static str, &'static File<'static>> = BTreeMap::new();
    for entry in folder.entries() {
        let file_name = file_name(entry.path())?;
        if is_skipped(file_name) {
            continue;
        }
        let DirEntry::File(file) = entry else {
            bail!("{}: unknown folder", at(file_name));
        };
        files.insert(file_name, file);
    }
    let mut used: HashSet<&str> = HashSet::new();

    let pack_file = files
        .get(PACK_FILE)
        .with_context(|| format!("{}: missing", at(PACK_FILE)))?;
    used.insert(PACK_FILE);
    let pack_at = at(PACK_FILE);
    let pack: PackFile = toml::from_str(utf8(pack_file)?).with_context(|| pack_at.clone())?;
    ensure!(
        !pack.description.trim().is_empty(),
        "{pack_at}: `description` is empty"
    );
    let args = load_args(&pack_at, pack.args)?;

    let config_files: Vec<&str> = files
        .keys()
        .copied()
        .filter(|file_name| {
            *file_name == LUA_CONFIG_FILE
                || EXTENSIONS
                    .iter()
                    .any(|ext| *file_name == format!("config.{ext}"))
        })
        .collect();
    ensure!(
        config_files.len() <= 1,
        "packs/{}: more than one config file: {}",
        folder.path().display(),
        config_files.join(", ")
    );
    let config = match config_files.first() {
        Some(&LUA_CONFIG_FILE) => {
            used.insert(LUA_CONFIG_FILE);
            Some(PackConfig::Lua(utf8(files[LUA_CONFIG_FILE])?))
        }
        Some(&file_name) => {
            used.insert(file_name);
            Some(PackConfig::Static(document(files[file_name])?))
        }
        None => None,
    };

    let setup = match files.get(SETUP_FILE) {
        Some(file) => {
            used.insert(SETUP_FILE);
            Some(utf8(file)?)
        }
        None => None,
    };

    if let Some(file_name) = files.keys().find(|file_name| !used.contains(*file_name)) {
        bail!("{}: unknown file", at(file_name));
    }

    Ok(PackVersionData {
        metadata: PackMetadata {
            name: name.to_string(),
            version: version.to_string(),
            label: pack.label,
            description: pack.description,
            kind: pack.kind,
            has_setup: setup.is_some(),
        },
        args,
        config,
        setup,
    })
}

/// Check the `[[args]]` of the file `at`: unique keys, the fields of
/// their type, and a default of that type.
fn load_args(at: &str, entries: Vec<ArgEntry>) -> anyhow::Result<Vec<PackArg>> {
    let mut args: Vec<PackArg> = Vec::new();
    for entry in entries {
        let arg_at = format!("{at}: arg `{}`", entry.key);
        ensure!(
            is_identifier(&entry.key) && !RESERVED_ARG_KEYS.contains(&entry.key.as_str()),
            "{arg_at}: the key must match [a-z][a-z0-9-]* and not be `version`, `enabled` or \
             `args`"
        );
        ensure!(
            !args.iter().any(|arg| arg.key == entry.key),
            "{arg_at}: the key is used twice"
        );
        let kind = match (entry.r#type, entry.values) {
            (ArgType::Bool, None) if entry.other.is_none() => ArgKind::Bool,
            (ArgType::Bool, _) => bail!("{arg_at}: `values` and `other` are only for a choice"),
            (ArgType::Choice, None) => bail!("{arg_at}: a choice needs `values`"),
            (ArgType::Choice, Some(values)) => {
                ensure!(!values.is_empty(), "{arg_at}: `values` is empty");
                let mut seen = HashSet::new();
                for value in &values {
                    ensure!(
                        seen.insert(value.as_str()),
                        "{arg_at}: the value `{value}` is in `values` twice"
                    );
                }
                ArgKind::Choice {
                    values,
                    other: entry.other.unwrap_or(false),
                }
            }
        };
        let default = kind
            .parse(&entry.default)
            .map_err(|e| anyhow!("{arg_at}: default {e}"))?;
        args.push(PackArg {
            key: entry.key,
            description: entry.description,
            kind,
            default,
        });
    }
    Ok(args)
}

/// A static config file of a version folder: a table that does not set
/// `packs` or `presets`.
fn document(file: &'static File<'static>) -> anyhow::Result<Value> {
    let path = Path::new("packs").join(file.path());
    let value = parse_file(&path, utf8(file)?)?;
    ensure!(
        value.is_object(),
        "{}: a pack config must be a table",
        path.display()
    );
    ensure!(
        value.get("packs").is_none() && value.get("presets").is_none(),
        "{}: a pack config cannot set `packs` or `presets`",
        path.display()
    );
    Ok(value)
}

fn file_name(path: &'static Path) -> anyhow::Result<&'static str> {
    path.file_name()
        .and_then(|name| name.to_str())
        .with_context(|| format!("packs/{}: the name is not UTF-8", path.display()))
}

fn utf8(file: &'static File<'static>) -> anyhow::Result<&'static str> {
    file.contents_utf8()
        .with_context(|| format!("packs/{}: not UTF-8", file.path().display()))
}

/// Hidden files and editor backups (`.DS_Store`, `config.toml~`).
fn is_skipped(file_name: &str) -> bool {
    file_name.starts_with('.') || file_name.ends_with('~')
}

/// `text` matches `^[a-z][a-z0-9-]*$`.
fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// A whole number from 1 without leading zeros.
fn is_version(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_digit())
        && !text.starts_with('0')
        && text.parse::<u64>().is_ok()
}

/// The built-in packs and the test pack `sample@1` (a tool): a choice
/// arg `mode` (`fast` or `slow`, or any other value; `broken` fails), a
/// bool arg `network`, a `config.lua` and a setup script. Loaded once
/// per test process: each load leaks its embedded files.
#[cfg(test)]
pub fn load_with_sample() -> PackManager {
    static SAMPLE: std::sync::LazyLock<PackManager> =
        std::sync::LazyLock::new(|| load_sample().expect("the sample pack loads"));
    SAMPLE.clone()
}

#[cfg(test)]
fn load_sample() -> anyhow::Result<PackManager> {
    const PACK: &str = r#"
label = "Sample"
description = "A test pack with args and a Lua config"
kind = "tool"

[[args]]
key = "mode"
type = "choice"
description = "Mode"
default = "fast"
values = ["fast", "slow"]
other = true

[[args]]
key = "network"
type = "bool"
description = "Allow sample.example"
default = true
"#;
    const CONFIG: &str = r#"
if pack.args.mode == "broken" then
    fail("mode `broken` is not supported; use `mode = \"fast\"`")
end
config.mounts = {
    ["sample-dir"] = { source = "~/.airlock/sample", target = "~/.sample", missing = "create-dir" },
}
config.env = { SAMPLE_MODE = pack.args.mode }
if pack.args.network then
    config.network = { rules = { sample = { allow = { "sample.example:443" } } } }
end
"#;
    let files = vec![
        ("sample@1/pack.toml", PACK.to_string()),
        ("sample@1/config.lua", CONFIG.to_string()),
        ("sample@1/setup.sh", "true\n".to_string()),
    ];
    load(fixture(BUILTIN_PACKS.entries().to_vec(), files))
}

/// An embedded directory of `root` and `files` (`<folder>/<file>`,
/// `<file>`, or deeper paths for nested folders).
#[cfg(test)]
pub(super) fn fixture(
    mut root: Vec<DirEntry<'static>>,
    files: Vec<(&'static str, String)>,
) -> &'static Dir<'static> {
    root.extend(fixture_entries("", files));
    Box::leak(Box::new(Dir::new("", root.leak())))
}

/// The entries of the folder `prefix` (with a trailing `/`; `""` for the
/// root) made of `files`.
#[cfg(test)]
fn fixture_entries(prefix: &str, files: Vec<(&'static str, String)>) -> Vec<DirEntry<'static>> {
    let mut entries = Vec::new();
    let mut folders: BTreeMap<&'static str, Vec<(&'static str, String)>> = BTreeMap::new();
    for (path, text) in files {
        match path[prefix.len()..].split_once('/') {
            Some((folder, _)) => folders
                .entry(&path[..prefix.len() + folder.len()])
                .or_default()
                .push((path, text)),
            None => entries.push(DirEntry::File(File::new(path, text.leak().as_bytes()))),
        }
    }
    for (folder, files) in folders {
        let children = fixture_entries(&format!("{folder}/"), files);
        entries.push(DirEntry::Dir(Dir::new(folder, children.leak())));
    }
    entries
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packs::ArgValue;

    const PACK: &str = r#"
label = "Demo"
description = "Installs the demo"
kind = "tool"

[[args]]
key = "mode"
type = "choice"
description = "Mode"
default = "a"
values = ["a", "b"]
"#;

    fn load_files(files: &[(&'static str, &str)]) -> anyhow::Result<PackManager> {
        let files = files
            .iter()
            .map(|(path, text)| (*path, (*text).to_string()))
            .collect();
        load(fixture(vec![], files))
    }

    fn versions(packs: &[Pack]) -> Vec<String> {
        packs
            .iter()
            .map(|p| format!("{}@{}", p.metadata().name, p.metadata().version))
            .collect()
    }

    fn pack_of_kind(kind: &str) -> String {
        PACK.replace("kind = \"tool\"", &format!("kind = \"{kind}\""))
    }

    #[test]
    fn builtin_packs_load() {
        let packs = crate::packs::init().unwrap();
        assert_eq!(
            versions(&packs.builtin()),
            [
                "alpine@1",
                "debian@1",
                "claude@1",
                "codex@1",
                "copilot@1",
                "docker@1",
                "git@1",
                "mise@1",
                "nodejs@1",
                "python@1",
                "rust@1",
            ]
        );
    }

    #[test]
    fn loading_packs_orders_them_by_kind_then_name_and_skips_hidden_files() {
        let distro = pack_of_kind("distro");
        let agent = pack_of_kind("agent");
        let packs = load_files(&[
            (".DS_Store", "x"),
            ("zeta@1/pack.toml", PACK),
            ("demo@2/pack.toml", PACK),
            ("demo@1/pack.toml", PACK),
            ("demo@1/.DS_Store", "x"),
            ("demo@1/config.toml~", "x"),
            ("bot@1/pack.toml", &agent),
            ("os@1/pack.toml", &distro),
        ])
        .unwrap();
        assert_eq!(
            versions(&packs.packs),
            ["os@1", "bot@1", "demo@1", "demo@2", "zeta@1"]
        );
        assert_eq!(
            versions(&packs.builtin()),
            ["os@1", "bot@1", "demo@2", "zeta@1"]
        );
    }

    #[test]
    fn loading_pack_folder_reads_metadata_args_config_and_setup() {
        for (file, text) in [
            ("demo@1/config.toml", "cpus = 2\n"),
            ("demo@1/config.json", "{ \"cpus\": 2 }"),
            ("demo@1/config.yaml", "cpus: 2\n"),
        ] {
            let packs = load_files(&[
                ("demo@1/pack.toml", PACK),
                (file, text),
                ("demo@1/setup.sh", "true\n"),
            ])
            .unwrap();
            let demo = &packs.packs[0].0;
            assert_eq!(demo.metadata.label, "Demo");
            assert_eq!(demo.metadata.description, "Installs the demo");
            assert_eq!(demo.metadata.kind, PackKind::Tool);
            assert!(demo.metadata.has_setup);
            assert_eq!(demo.setup, Some("true\n"));
            assert!(
                matches!(&demo.config, Some(PackConfig::Static(value)) if value["cpus"] == 2),
                "{file}"
            );
            assert_eq!(demo.args[0].key, "mode");
            assert_eq!(demo.args[0].default, ArgValue::Text("a".into()));
        }
        let packs = load_files(&[
            ("demo@1/pack.toml", PACK),
            ("demo@1/config.lua", "config.cpus = 2\n"),
        ])
        .unwrap();
        let demo = &packs.packs[0].0;
        assert!(matches!(
            demo.config,
            Some(PackConfig::Lua("config.cpus = 2\n"))
        ));
        assert!(!demo.metadata.has_setup && demo.setup.is_none());
    }

    #[test]
    fn loading_invalid_pack_folder_fails_naming_problem() {
        let mut cases: Vec<(&'static str, Option<String>, String)> = vec![
            (
                "demo@1/notes.txt",
                Some("x".into()),
                "packs/demo@1/notes.txt: unknown file".into(),
            ),
            (
                "readme.md",
                Some("x".into()),
                "packs/readme.md: unknown file".into(),
            ),
            (
                "demo@1/sub/config.toml",
                Some("cpus = 1\n".into()),
                "packs/demo@1/sub: unknown folder".into(),
            ),
            (
                "demo@1/config.yaml",
                Some("cpus: 2\n".into()),
                "more than one config file".into(),
            ),
            (
                "demo@1/config.lua",
                Some("config.cpus = 2\n".into()),
                "more than one config file".into(),
            ),
            (
                "demo@1/config.toml",
                Some("packs = [\"python\"]\n".into()),
                "packs/demo@1/config.toml: a pack config cannot set `packs`".into(),
            ),
            (
                "demo@2/pack.toml",
                Some(PACK.replace("label = \"Demo\"", "label = \"Other\"")),
                "the label \"Other\" differs from \"Demo\"".into(),
            ),
            (
                "demo@2/pack.toml",
                Some(pack_of_kind("agent")),
                "the kind differs from the kind of the other versions".into(),
            ),
            (
                "demo@1/pack.toml",
                None,
                "packs/demo@1/pack.toml: missing".into(),
            ),
        ];
        let pack_cases = [
            (
                PACK.replace("default = \"a\"", "default = \"c\""),
                "arg `mode`: default `c` is not valid (one of: a, b)",
            ),
            (
                PACK.replace("description = \"Installs the demo\"", "description = \" \""),
                "packs/demo@1/pack.toml: `description` is empty",
            ),
            (
                format!(
                    "{PACK}\n[[args]]\nkey = \"mode\"\ndescription = \"Again\"\ntype = \
                     \"choice\"\nvalues = [\"x\"]\ndefault = \"x\"\n"
                ),
                "arg `mode`: the key is used twice",
            ),
            (
                PACK.replace("values = [\"a\", \"b\"]", "values = []"),
                "arg `mode`: `values` is empty",
            ),
            (
                PACK.replace("values = [\"a\", \"b\"]", "values = [\"a\", \"b\", \"a\"]"),
                "arg `mode`: the value `a` is in `values` twice",
            ),
            (
                PACK.replace("values = [\"a\", \"b\"]\n", ""),
                "arg `mode`: a choice needs `values`",
            ),
            (
                PACK.replace("type = \"choice\"", "type = \"bool\""),
                "arg `mode`: `values` and `other` are only for a choice",
            ),
        ];
        for (pack, expected) in pack_cases {
            cases.push(("demo@1/pack.toml", Some(pack), expected.into()));
        }
        for key in ["enabled", "version", "args", "Mode", "2mode", "mo_de"] {
            cases.push((
                "demo@1/pack.toml",
                Some(PACK.replace("key = \"mode\"", &format!("key = \"{key}\""))),
                format!(
                    "arg `{key}`: the key must match [a-z][a-z0-9-]* and not be `version`, \
                     `enabled` or `args`"
                ),
            ));
        }
        for folder in [
            "demo",
            "Demo@1",
            "demo@01",
            "demo@0",
            "demo@",
            "demo@v1",
            "demo@legacy",
        ] {
            cases.push((
                format!("{folder}/pack.toml").leak(),
                Some(PACK.into()),
                format!("packs/{folder}: the folder name must be <name>@<version>"),
            ));
        }

        for (path, text, expected) in cases {
            let mut files: Vec<(&'static str, String)> = vec![
                ("demo@1/pack.toml", PACK.into()),
                ("demo@1/config.toml", "cpus = 2\n".into()),
                ("demo@1/setup.sh", "true\n".into()),
            ];
            files.retain(|(p, _)| *p != path);
            files.extend(text.map(|text| (path, text)));
            let e = match load(fixture(vec![], files)) {
                Ok(_) => panic!("loads: {expected}"),
                Err(e) => format!("{e:#}"),
            };
            assert!(e.contains(&expected), "{expected}: {e}");
        }
    }
}
