//! Shared host-side helpers.
//!
//! Small helpers for paths, environment variables, executables,
//! terminal-safe text and JSON files. Also gives file access that does not
//! follow symlinks.

pub(crate) mod safe_fs;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) use safe_fs::PinnedDir;

/// Expand a `~` prefix in a path string to `home`.
pub(crate) fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Apply overrides to a `KEY=VALUE` environment and remove keys.
///
/// This is the single precedence rule for all guest environments: `[env]`
/// over the image env, daemon env over the sandbox env, and
/// `airlock exec -e` over the sandbox env.
/// Args:
///  - `base`: Base environment as `KEY=VALUE` items
///  - `overrides`: `(key, value)` pairs that replace the same keys in `base`
///  - `unset`: Keys to remove from the result
///
/// Returns:
///   The merged environment. Keys without an override keep their base
///   order. Each override replaces the old entry and goes to the end.
pub(crate) fn merge_env<K, V>(
    base: &[String],
    overrides: impl IntoIterator<Item = (K, V)>,
    unset: &[String],
) -> Vec<String>
where
    K: AsRef<str>,
    V: AsRef<str>,
{
    let mut out = base.to_vec();
    for (key, value) in overrides {
        let (key, value) = (key.as_ref(), value.as_ref());
        out.retain(|e| !has_key(e, key));
        out.push(format!("{key}={value}"));
    }
    out.retain(|e| !unset.iter().any(|key| has_key(e, key)));
    out
}

/// Return `true` if the `KEY=VALUE` entry `entry` sets `key`.
fn has_key(entry: &str, key: &str) -> bool {
    entry
        .strip_prefix(key)
        .is_some_and(|rest| rest.starts_with('='))
}

/// Return `true` if `program` is an executable file on `PATH`.
pub(crate) fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

/// Return `true` if `path` is a regular file with an execute bit set.
pub(crate) fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Make guest text safe to print on the host terminal. Guest output is
/// untrusted and must not move the cursor, change the window title or hide
/// text.
/// Returns:
///   `bytes` as lossy UTF-8 without escape sequences (CSI, OSC, other
///   `ESC x` pairs) and control characters, except tab and newline.
pub(crate) fn strip_controls(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.next() {
                // CSI: parameters and intermediates up to a final byte.
                Some('[') => {
                    for c in chars.by_ref() {
                        if ('\u{40}'..='\u{7e}').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC / DCS / APC / PM / SOS: up to BEL or ST (ESC \).
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' {
                            break;
                        }
                        if c == '\u{1b}' && chars.peek() == Some(&'\\') {
                            chars.next();
                            break;
                        }
                    }
                }
                _ => {}
            },
            '\t' | '\n' => out.push(c),
            c if c.is_control() => {}
            c => out.push(c),
        }
    }
    out
}

/// Read and parse the JSON file `name` in `dir`.
/// Args:
///  - `dir`: Directory of the file
///  - `name`: File name
///  - `cap`: Maximum file size in bytes
///
/// Returns:
///   The parsed value, `Ok(None)` if the file does not exist, or error if
///   the read or parse fails.
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(
    dir: &PinnedDir,
    name: &str,
    cap: u64,
) -> anyhow::Result<Option<T>> {
    let Some(bytes) = dir.read(name, cap)? else {
        return Ok(None);
    };
    let value = serde_json::from_slice(&bytes)
        .map_err(|e| anyhow::anyhow!("parse {}: {e}", dir.path().join(name).display()))?;
    Ok(Some(value))
}

/// Write `value` as pretty JSON to the file `name` in `dir`. The write is
/// atomic and the file gets the permission bits `mode`.
pub(crate) fn write_json<T: serde::Serialize>(
    dir: &PinnedDir,
    name: &str,
    value: &T,
    mode: u32,
) -> anyhow::Result<()> {
    let mut json = serde_json::to_vec_pretty(value)?;
    json.push(b'\n');
    dir.write_atomic(name, &json, mode)
        .map_err(|e| anyhow::anyhow!("write {}: {e}", dir.path().join(name).display()))
}

#[cfg(test)]
mod tests {
    //! Tests for the env merge, the `PATH` lookup and the cleanup of guest
    //! text.

    use super::*;

    /// The `entries` as owned strings.
    fn env(entries: &[&str]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    /// Test the precedence rule of all guest environments: an override
    /// replaces the whole key and goes to the end, and unset keys go away.
    ///   1. Merge a base env with overrides and keys to unset
    ///   2. Check that `FOOBAR` stays (a key prefix does not match), the
    ///      overrides are last in their order, and the unset keys are gone,
    ///      also an override that is also unset
    #[test]
    fn merging_env_replaces_whole_keys_appends_overrides_and_drops_unset() {
        let base = env(&["PATH=/bin", "HOME=/root", "FOO=1", "FOOBAR=2", "B=x"]);
        let out = merge_env(
            &base,
            [("HOME", "/tmp"), ("FOO", "3"), ("NEW", "1"), ("D", "4")],
            &env(&["B", "D", "MISSING"]),
        );
        assert_eq!(
            out,
            env(&["PATH=/bin", "FOOBAR=2", "HOME=/tmp", "FOO=3", "NEW=1"])
        );
    }

    /// Test that the `PATH` lookup finds only executable files.
    ///   1. Check that `sh` is found and a missing program is not
    ///   2. Check that a directory is not executable
    #[test]
    fn on_path_finds_executable_files_only() {
        assert!(on_path("sh"));
        assert!(!on_path("airlock-definitely-not-a-real-program"));
        assert!(!is_executable(Path::new("/")));
    }

    /// Test that guest text loses all terminal escapes and control
    /// characters except tab and newline, so that the guest cannot change
    /// the host terminal.
    ///   1. Strip CSI color codes, OSC title and link sequences, carriage
    ///      returns, backspaces, NUL and a C1 control
    ///   2. Check that bad UTF-8 becomes a replacement character
    ///   3. Check that a lone ESC at the end goes away
    #[test]
    fn stripping_guest_text_removes_escapes_and_controls() {
        assert_eq!(strip_controls(b"plain\ttext\n"), "plain\ttext\n");
        assert_eq!(strip_controls(b"\x1b[1;31mred\x1b[0m"), "red");
        assert_eq!(
            strip_controls(b"\x1b]0;title\x07after \x1b]8;;http://x\x1b\\link"),
            "after link"
        );
        assert_eq!(strip_controls(b"50%\r100%\x08\x00"), "50%100%");
        assert_eq!(strip_controls("\u{9b}c1".as_bytes()), "c1");
        assert_eq!(strip_controls(b"bad \xff utf8"), "bad \u{fffd} utf8");
        assert_eq!(strip_controls(b"end\x1b"), "end");
    }
}
