//! Pack file snapshots.
//!
//! A snapshot keeps a copy of the files of a pack at one time, so that
//! airlock can show later what changed in the pack. Each file has its hash.
//! A small text file also has its text. A large or binary file has only its
//! hash, so the snapshot shows that it changed, but not how.
//!
//! The snapshot of an install also keeps the version, the arg values and a
//! keyed hash of each setup value. Thus airlock can show which args and
//! which setup values changed since the install, but never the setup values.
//!
//! The diff lines of two snapshots are safe for the terminal.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::prompt;
use crate::cli::prompt::style::{self, Styles};
use crate::messages;

/// Maximum size of a file text in a snapshot, in bytes. A larger file keeps
/// only its hash. It is the same as the size limit of a pack file on the
/// disk, so each text file shows in full.
pub const MAX_TEXT_BYTES: usize = 256 * 1024;

/// A copy of the files of one pack.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackSnapshot {
    /// Pack id, as the `[packs]` table has it.
    pub id: String,
    /// The files of the pack, by file name.
    pub files: BTreeMap<String, FileSnapshot>,
    /// Version of a built-in pack. Only install snapshots have it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// Arg values, by key. Only install snapshots have them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub args: BTreeMap<String, String>,
    /// Keyed hash of each setup value as hex, by key (see
    /// [`crate::packs::SetupEnv::key_macs`]). Only install snapshots have
    /// them.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env_keys: BTreeMap<String, String>,
}

/// One file of a [`PackSnapshot`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileSnapshot {
    /// SHA-256 of the file content, as hex.
    pub sha256: String,
    /// Text of the file. `None` if the file is larger than
    /// [`MAX_TEXT_BYTES`] or is not UTF-8.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

impl PackSnapshot {
    /// Make the snapshot of the pack files `files`.
    /// Args:
    ///  - `id`: Pack id
    ///  - `files`: The contents of the files, by file name
    pub fn new<'a>(id: &str, files: impl IntoIterator<Item = (&'a str, &'a [u8])>) -> Self {
        let files = files
            .into_iter()
            .map(|(name, bytes)| (name.to_string(), FileSnapshot::new(bytes)))
            .collect();
        Self {
            id: id.to_string(),
            files,
            ..Self::default()
        }
    }

    /// Remove the texts of the files. The hashes stay.
    pub fn drop_texts(&mut self) {
        for file in self.files.values_mut() {
            file.text = None;
        }
    }
}

impl FileSnapshot {
    /// Make the snapshot of one file with the content `bytes`.
    pub fn new(bytes: &[u8]) -> Self {
        let text = (bytes.len() <= MAX_TEXT_BYTES)
            .then(|| std::str::from_utf8(bytes).ok())
            .flatten()
            .map(ToString::to_string);
        Self {
            sha256: hex::encode(Sha256::digest(bytes)),
            text,
        }
    }
}

/// Return the lines that show the changed files from `last` to `new` as a
/// diff. Unchanged files do not show. All text from the files is made safe
/// for the terminal (see [`style::escape_controls`]). If `last` or `new`
/// has only the hash of a changed file, a note tells that the changes
/// cannot show.
pub(crate) fn file_lines(last: &PackSnapshot, new: &PackSnapshot, styles: &Styles) -> Vec<String> {
    let mut names: Vec<&String> = new.files.keys().collect();
    names.extend(last.files.keys().filter(|n| !new.files.contains_key(*n)));
    names.sort();
    let mut lines = Vec::new();
    for name in names {
        let shown = style::escape_controls(name, false);
        let (before, after) = (last.files.get(name), new.files.get(name));
        let header = match (before, after) {
            (Some(a), Some(b)) if a.sha256 == b.sha256 => continue,
            (Some(_), Some(_)) => shown.to_string(),
            (None, _) => messages::packs::snapshot_file_added(&shown),
            (Some(_), None) => messages::packs::snapshot_file_removed(&shown),
        };
        lines.push(styles.cyan.apply_to(header).to_string());
        // A missing file has no text. A file with only a hash cannot show.
        let (Some(before), Some(after)) = (text(before), text(after)) else {
            lines.push(
                styles
                    .dim
                    .apply_to(format!("  {}", messages::packs::CHANGES_NOT_SHOWN))
                    .to_string(),
            );
            continue;
        };
        lines.extend(prompt::diff::unified(
            &style::escape_controls(before, true),
            &style::escape_controls(after, true),
            styles,
        ));
    }
    lines
}

/// Return the text of `file`: empty for no file, `None` for a file with
/// only a hash.
fn text(file: Option<&FileSnapshot>) -> Option<&str> {
    file.map_or(Some(""), |f| f.text.as_deref())
}

/// Return the lines that show the change of the version and the arg values
/// from `last` to `new`, as a diff. No lines if they are the same.
pub(crate) fn args_lines(last: &PackSnapshot, new: &PackSnapshot, styles: &Styles) -> Vec<String> {
    if last.version == new.version && last.args == new.args {
        return Vec::new();
    }
    let text = |snapshot: &PackSnapshot| {
        let lines: Vec<String> = snapshot
            .version
            .iter()
            .map(|version| format!("version = {version}\n"))
            .chain(
                snapshot
                    .args
                    .iter()
                    .map(|(key, value)| format!("{key} = {value}\n")),
            )
            .collect();
        style::escape_controls(&lines.concat(), true).into_owned()
    };
    let mut lines = vec![
        styles
            .cyan
            .apply_to(messages::packs::SNAPSHOT_ARGS_HEADER)
            .to_string(),
    ];
    lines.extend(prompt::diff::unified(&text(last), &text(new), styles));
    lines
}

/// Return the setup value keys that changed from `last` to `new`: new
/// keys, removed keys and keys with another value, sorted.
pub(crate) fn changed_env_keys<'a>(last: &'a PackSnapshot, new: &'a PackSnapshot) -> Vec<&'a str> {
    let mut keys: Vec<&str> = last
        .env_keys
        .keys()
        .chain(new.env_keys.keys())
        .filter(|key| last.env_keys.get(*key) != new.env_keys.get(*key))
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    keys.dedup();
    keys
}

#[cfg(test)]
mod tests {
    //! Tests of the snapshot diff lines.

    use super::*;

    /// Test that the lines show only the changed files as a diff, and a
    /// note for a changed file whose old copy has only a hash. The user
    /// must see what changed since the install.
    ///   1. Make an old copy with three files and a new copy that changes
    ///      one, removes one and adds one
    ///   2. Check the headers and the diff lines
    ///   3. Remove the old texts and check that the changed file shows the
    ///      note
    #[test]
    fn changed_files_show_as_diff_or_note() {
        let mut last = PackSnapshot::new(
            "~p",
            [
                ("pack.toml", b"label = \"P\"\n".as_slice()),
                ("config.lua", b"a = 1\n".as_slice()),
                ("setup.sh", b"echo hi\n".as_slice()),
            ],
        );
        let new = PackSnapshot::new(
            "~p",
            [
                ("pack.toml", b"label = \"P\"\n".as_slice()),
                ("config.lua", b"a = 2\n".as_slice()),
                ("README", b"x\n".as_slice()),
            ],
        );
        let lines = |last: &PackSnapshot| -> Vec<String> {
            file_lines(last, &new, &Styles::new())
                .iter()
                .map(|line| console::strip_ansi_codes(line).into_owned())
                .collect()
        };
        assert_eq!(
            lines(&last),
            [
                "README (new file)",
                "+ x",
                "config.lua",
                "- a = 1",
                "+ a = 2",
                "setup.sh (removed)",
                "- echo hi",
            ]
        );
        last.drop_texts();
        let note = format!("  {}", messages::packs::CHANGES_NOT_SHOWN);
        let note = note.as_str();
        assert_eq!(
            lines(&last),
            [
                "README (new file)",
                "+ x",
                "config.lua",
                note,
                "setup.sh (removed)",
                note,
            ]
        );
    }
}
