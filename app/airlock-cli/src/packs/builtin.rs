//! Built-in packs.
//!
//! Embeds the packs of the repository in the binary. Loads them and checks
//! that each pack version is valid.

use std::collections::BTreeMap;
use std::sync::Arc;

use anyhow::{Context, bail, ensure};
use include_dir::{Dir, DirEntry, include_dir};

use crate::packs::id::is_identifier;
use crate::packs::loader::{PACK_FILE, is_skipped, load_pack};
use crate::packs::{Pack, PackId, PackManager, PackVersionData};

/// The `packs/` directory of the repository, embedded in the binary.
/// Each folder `<name>@<version>/` is one pack version.
pub static BUILTIN_PACKS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/../../packs");

/// Load and validate the packs of a `packs/` directory.
///
/// Each directory `<name>@<version>/` is one pack version (see
/// [`load_pack`] for its files). Hidden files and editor backups are
/// ignored.
/// Args:
///  - `root`: The `packs/` directory, for example [`BUILTIN_PACKS`]
///
/// Returns:
///   The packs, sorted by kind (see [`crate::packs::PackKind`]), then by
///   name. An error if a pack is not valid.
pub fn load(root: &Dir<'_>) -> anyhow::Result<PackManager> {
    // The versions of each pack, by name.
    let mut by_name: BTreeMap<&str, Vec<(u64, PackVersionData)>> = BTreeMap::new();
    for entry in root.entries() {
        let file_name = file_name(entry)?;
        if is_skipped(file_name) {
            continue;
        }
        let DirEntry::Dir(folder) = entry else {
            bail!(crate::messages::packs::pack_root_file_unknown(file_name));
        };
        let (name, version) = file_name
            .split_once('@')
            .filter(|(name, version)| is_identifier(name) && is_version(version))
            .with_context(|| crate::messages::packs::pack_folder_name_invalid(file_name))?;
        let number = version.parse::<u64>()?;
        let data = load_version(name, version, folder)?;
        by_name.entry(name).or_default().push((number, data));
    }

    let mut groups: Vec<Vec<PackVersionData>> = Vec::new();
    for (name, mut versions) in by_name {
        versions.sort_by_key(|(number, _)| *number);
        let first = &versions[0].1.metadata;
        for (number, data) in &versions[1..] {
            let at = format!("packs/{name}@{number}/{PACK_FILE}");
            ensure!(
                data.metadata.label == first.label,
                crate::messages::packs::pack_label_differs(&at, &data.metadata.label, &first.label)
            );
            ensure!(
                data.metadata.kind == first.kind,
                crate::messages::packs::pack_kind_differs(&at)
            );
        }
        groups.push(versions.into_iter().map(|(_, data)| data).collect());
    }
    // The sort is stable, thus the names stay in order within a kind.
    groups.sort_by_key(|versions| versions[0].metadata.kind);
    Ok(PackManager::new(
        groups
            .into_iter()
            .flatten()
            .map(|data| Pack(Arc::new(data)))
            .collect(),
    ))
}

/// Read the files of the directory of one pack version and load them.
fn load_version(name: &str, version: &str, dir: &Dir<'_>) -> anyhow::Result<PackVersionData> {
    let at = format!("packs/{}", dir.path().display());
    let mut files: BTreeMap<&str, &[u8]> = BTreeMap::new();
    for entry in dir.entries() {
        let file_name = file_name(entry)?;
        if is_skipped(file_name) {
            continue;
        }
        let DirEntry::File(file) = entry else {
            bail!(crate::messages::packs::pack_folder_unknown(format!(
                "{at}/{file_name}"
            )));
        };
        files.insert(file_name, file.contents());
    }
    load_pack(
        &at,
        PackId::builtin(name),
        Some(version.to_string()),
        &files,
    )
}

/// The file name of `entry` as UTF-8.
fn file_name<'a>(entry: &'a DirEntry<'_>) -> anyhow::Result<&'a str> {
    let path = entry.path();
    path.file_name()
        .and_then(|name| name.to_str())
        .with_context(|| crate::messages::packs::pack_file_name_not_utf8(path.display()))
}

/// Check if `text` is a whole number from 1 without leading zeros.
fn is_version(text: &str) -> bool {
    text.bytes().all(|b| b.is_ascii_digit())
        && !text.starts_with('0')
        && text.parse::<u64>().is_ok()
}

/// The built-in packs and the test pack `sample@1` (a tool). The sample
/// pack has a choice arg `mode` (`fast`, `slow` or any other value, but
/// `broken` fails), a bool arg `network`, a `config.lua` and a setup
/// script. The packs load once per test process because each load leaks
/// its embedded files.
#[cfg(test)]
pub fn load_with_sample() -> PackManager {
    static SAMPLE: std::sync::LazyLock<PackManager> =
        std::sync::LazyLock::new(|| load_sample().expect("the sample pack loads"));
    SAMPLE.clone()
}

/// Load the built-in packs and the test pack `sample@1`, see
/// [`load_with_sample`].
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

/// The entries of the folder `prefix` (with a trailing `/`, or `""` for
/// the root) made of `files`.
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
            None => entries.push(DirEntry::File(include_dir::File::new(
                path,
                text.leak().as_bytes(),
            ))),
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
    //! Tests of how the pack loader reads and checks a `packs/` directory.

    use super::*;
    use crate::packs::{ArgValue, PackConfig, PackKind};

    /// A `pack.toml` of a tool pack with one choice arg `mode`.
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

    /// Load the packs of an in-memory `packs/` directory of `files`.
    fn load_files(files: &[(&'static str, &str)]) -> anyhow::Result<PackManager> {
        let files = files
            .iter()
            .map(|(path, text)| (*path, (*text).to_string()))
            .collect();
        load(fixture(vec![], files))
    }

    /// The `<name>@<version>` of each pack, in order.
    fn versions(packs: &[Pack]) -> Vec<String> {
        packs
            .iter()
            .map(|p| {
                let metadata = p.metadata();
                format!(
                    "{}@{}",
                    metadata.id,
                    metadata.version.as_deref().unwrap_or("-")
                )
            })
            .collect()
    }

    /// [`PACK`] with the kind `kind`.
    fn pack_of_kind(kind: &str) -> String {
        PACK.replace("kind = \"tool\"", &format!("kind = \"{kind}\""))
    }

    /// Test that the built-in packs load and that the newest versions come
    /// in kind order, then name order.
    ///   1. Load the built-in packs
    ///   2. Check the list of newest versions
    #[test]
    fn builtin_packs_load() {
        let packs = crate::packs::init_builtin().unwrap();
        assert_eq!(
            versions(&packs.offered()),
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

    /// Test that the loader sorts packs by kind, then name, then version,
    /// and skips hidden and backup files. A pack with no kind is an
    /// "other" pack and comes last.
    ///   1. Load packs of three kinds, a pack with no kind, two versions of
    ///      one pack, and hidden and backup files
    ///   2. Check the order of all versions
    ///   3. Check that the offered list has only the newest version of
    ///      each pack
    ///   4. Check that the pack with no kind is an "other" pack
    #[test]
    fn loading_packs_orders_them_by_kind_then_name_and_skips_hidden_files() {
        let distro = pack_of_kind("distro");
        let agent = pack_of_kind("agent");
        let no_kind = PACK.replace("kind = \"tool\"\n", "");
        let packs = load_files(&[
            ("misc@1/pack.toml", &no_kind),
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
            ["os@1", "bot@1", "demo@1", "demo@2", "zeta@1", "misc@1"]
        );
        assert_eq!(
            versions(&packs.offered()),
            ["os@1", "bot@1", "demo@2", "zeta@1", "misc@1"]
        );
        assert_eq!(packs.packs[5].metadata().kind, PackKind::Other);
    }

    /// Test that the loader reads the metadata, args, config and setup
    /// script of a pack folder.
    ///   1. Load a pack with a TOML, JSON or YAML config and a setup script
    ///   2. Check the metadata, the setup script, the static config and the
    ///      args
    ///   3. Load a pack with a Lua config and no setup script
    ///   4. Check that the config is Lua and that there is no setup
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
            assert_eq!(demo.setup.as_deref(), Some("true\n"));
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
            &demo.config,
            Some(PackConfig::Lua(source)) if &**source == "config.cpus = 2\n"
        ));
        assert!(!demo.metadata.has_setup && demo.setup.is_none());
    }

    /// Test that the loader refuses a pack directory that is not valid, and
    /// that the error names the problem.
    ///   1. Make a list of cases: unknown files and folders, two configs,
    ///      a config that sets packs, versions that do not agree, a missing
    ///      `pack.toml`, bad args and bad folder names
    ///   2. For each case, change one file of a valid pack folder
    ///   3. Check that the load fails with the expected error text
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

        // Each case adds, replaces or (`None`) removes one file of a valid
        // pack folder.
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
