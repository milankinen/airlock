//! Pack identity.
//!
//! Each pack has an id. The id is the key of the pack in the `[packs]`
//! table and in the install records. The form of the id tells where the
//! pack comes from:
//!  * `name`: a pack that airlock has built in
//!  * `~name`: a user pack from the user pack dirs of the settings
//!  * `./path`: a project pack in a directory of the project

use std::borrow::Cow;
use std::fmt::{self, Write as _};
use std::sync::Arc;

use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};

/// The id of a pack. It is always valid (see [`PackRef::parse`]). Cheap to
/// clone.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PackId(Arc<str>);

impl PackId {
    /// Make the id of the built-in pack `name`.
    /// Args:
    ///  - `name`: Pack name. It must be an identifier (see [`is_identifier`]).
    pub fn builtin(name: &str) -> Self {
        debug_assert!(
            is_identifier(name),
            "pack name {name} must be an identifier"
        );
        Self(name.into())
    }

    /// Make the id of the user pack `name` (`~name`).
    /// Args:
    ///  - `name`: Directory name of the pack. It must be an identifier (see
    ///    [`is_identifier`]).
    pub fn local(name: &str) -> Self {
        debug_assert!(
            is_identifier(name),
            "pack name {name} must be an identifier"
        );
        Self(format!("{LOCAL_PREFIX}{name}").into())
    }

    /// Parse a key of the `[packs]` table.
    /// Args:
    ///  - `text`: The key, for example `claude`, `~my-tools` or
    ///    `./packs/setup`
    ///
    /// Returns:
    ///   The id, or `None` if `text` is not a valid pack id.
    pub fn parse(text: &str) -> Option<Self> {
        PackRef::parse(text).map(|_| Self(text.into()))
    }

    /// Get where the id points to.
    pub fn pack_ref(&self) -> PackRef<'_> {
        PackRef::parse(&self.0).expect("a pack id is always valid")
    }

    /// Get the id as text, as the `[packs]` table has it.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Get the directory name of the pack in the pack mount directory. A
    /// project pack gets a name for its project when it loads, so use
    /// [`crate::packs::PackMetadata::dir_name`] for a loaded pack.
    /// Returns:
    ///   The name for a built-in pack, `local.<name>` for a user pack, and
    ///   `project.<hash>` for a project pack. Here the hash is the first 16
    ///   hex digits of the SHA-256 of the path only. The `.` is not valid in
    ///   a pack name, so the names cannot clash.
    pub(super) fn dir_name(&self) -> Cow<'_, str> {
        match self.pack_ref() {
            PackRef::Builtin { name } => Cow::Borrowed(name),
            PackRef::Local { name } => Cow::Owned(format!("local.{name}")),
            PackRef::Project { path } => {
                let hash = hex::encode(Sha256::digest(path.as_bytes()));
                Cow::Owned(format!("project.{}", &hash[..16]))
            }
        }
    }
}

impl fmt::Display for PackId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl PartialEq<str> for PackId {
    fn eq(&self, other: &str) -> bool {
        *self.0 == *other
    }
}

impl PartialEq<&str> for PackId {
    fn eq(&self, other: &&str) -> bool {
        *self.0 == **other
    }
}

impl PartialEq<String> for PackId {
    fn eq(&self, other: &String) -> bool {
        *self.0 == **other
    }
}

impl Serialize for PackId {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0)
    }
}

/// Where a pack id points to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PackRef<'a> {
    /// A pack that airlock has built in, for example `claude`.
    Builtin {
        /// Pack name.
        name: &'a str,
    },
    /// A user pack from a user pack dir, for example `~my-tools`.
    Local {
        /// Directory name of the pack, with no `~`.
        name: &'a str,
    },
    /// A project pack in a directory of the project, for example
    /// `./packs/setup`.
    Project {
        /// Path of the pack directory, relative to the project root, with no
        /// `./`. The names of the path are separated with `/`.
        path: &'a str,
    },
}

/// The first character of a user pack id.
const LOCAL_PREFIX: char = '~';
/// The start of a project pack id.
pub const PROJECT_PREFIX: &str = "./";
/// Maximum length of a project pack id in bytes.
const MAX_PROJECT_ID_BYTES: usize = 256;

impl<'a> PackRef<'a> {
    /// Parse a key of the `[packs]` table.
    /// Args:
    ///  - `text`: The key, for example `claude`, `~my-tools` or
    ///    `./packs/setup`. A project pack path must be in the normal form
    ///    (see [`is_project_path`]).
    ///
    /// Returns:
    ///   The reference, or `None` if `text` is not a valid pack id.
    pub fn parse(text: &'a str) -> Option<Self> {
        if let Some(path) = text.strip_prefix(PROJECT_PREFIX) {
            return (text.len() <= MAX_PROJECT_ID_BYTES && is_project_path(path))
                .then_some(PackRef::Project { path });
        }
        match text.strip_prefix(LOCAL_PREFIX) {
            Some(name) => is_identifier(name).then_some(PackRef::Local { name }),
            None => is_identifier(text).then_some(PackRef::Builtin { name: text }),
        }
    }

    /// Return true if a `[packs]` entry of the pack must set `version`.
    pub fn needs_version(&self) -> bool {
        match self {
            PackRef::Builtin { .. } => true,
            PackRef::Local { .. } | PackRef::Project { .. } => false,
        }
    }
}

/// Write a `[packs]` key as a TOML key, for messages: a bare key if it is
/// an identifier, else a quoted key (`"~my-tools"`).
/// Args:
///  - `text`: The key as the config file has it. It can be any text, also
///    a key that is not a valid pack id.
pub fn toml_key(text: &str) -> Cow<'_, str> {
    if is_identifier(text) {
        return Cow::Borrowed(text);
    }
    // TOML basic string: escape `"`, `\` and control characters.
    let mut quoted = String::with_capacity(text.len() + 2);
    quoted.push('"');
    for c in text.chars() {
        match c {
            '"' => quoted.push_str("\\\""),
            '\\' => quoted.push_str("\\\\"),
            c if c.is_control() => {
                let _ = write!(quoted, "\\u{:04X}", u32::from(c));
            }
            c => quoted.push(c),
        }
    }
    quoted.push('"');
    Cow::Owned(quoted)
}

/// Check if `path` is a project pack path in the normal form. The path
/// cannot go out of the project: each name is a plain name, so there is
/// no `..`, `.`, empty name (`//`), hidden name or `/` at the start or end.
/// Args:
///  - `path`: The path after `./`
///
/// Returns:
///   True if each `/`-separated name matches `^[A-Za-z0-9_][A-Za-z0-9._-]*$`.
fn is_project_path(path: &str) -> bool {
    path.split('/').all(|name| {
        let mut chars = name.chars();
        chars
            .next()
            .is_some_and(|c| c.is_ascii_alphanumeric() || c == '_')
            && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
    })
}

/// Check if `text` is an identifier. Pack names and arg keys use this
/// form.
/// Args:
///  - `text`: The text to check
///
/// Returns:
///   True if `text` matches `^[a-z][a-z0-9-]*$`.
pub fn is_identifier(text: &str) -> bool {
    let mut chars = text.chars();
    chars.next().is_some_and(|c| c.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that only project pack paths in the normal form parse. The id
    /// is the key of the install records and of the mount directory, so
    /// one directory must have one id. A path must not go out of the project.
    ///   1. Parse paths in the normal form and check the path
    ///   2. Check that `..`, `.`, empty and hidden names, a `/` at the end,
    ///      and a path that is too long do not parse
    #[test]
    fn project_pack_path_parses_only_in_normal_form() {
        for (text, path) in [
            ("./packs/setup", "packs/setup"),
            ("./Tools/my_pack.v2", "Tools/my_pack.v2"),
        ] {
            assert_eq!(PackRef::parse(text), Some(PackRef::Project { path }));
        }
        let long = format!("./{}", "a".repeat(MAX_PROJECT_ID_BYTES));
        for text in [
            "./",
            "./..",
            "./../x",
            "./a/../b",
            "./a/./b",
            "./a//b",
            "./a/",
            "./.hidden",
            "././a",
            "./a b",
            "./a\\b",
            long.as_str(),
        ] {
            assert_eq!(PackRef::parse(text), None, "{text}");
        }
    }
}
