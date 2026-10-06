//! Small host-side helpers shared across modules.

pub(crate) mod safe_fs;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) use safe_fs::PinnedDir;

/// Expand `~` prefix in a path string.
pub(crate) fn expand_tilde(path: &str, home: &Path) -> PathBuf {
    if path == "~" {
        home.to_path_buf()
    } else if let Some(rest) = path.strip_prefix("~/") {
        home.join(rest)
    } else {
        PathBuf::from(path)
    }
}

/// Layer `(key, value)` overrides over a `KEY=VALUE` environment, then
/// drop every key named in `unset`.
///
/// An override replaces any prior entry with the same key and is appended
/// at the end, so the result keeps the base order for untouched keys. This
/// is the one precedence rule for every guest environment: `[env]` over
/// the image env, daemon env over the sandbox env, and `airlock exec -e`
/// over the sandbox env.
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

/// Whether the `KEY=VALUE` entry `entry` sets `key`.
fn has_key(entry: &str, key: &str) -> bool {
    entry
        .strip_prefix(key)
        .is_some_and(|rest| rest.starts_with('='))
}

/// Whether `program` resolves to an executable file on `PATH`.
pub(crate) fn on_path(program: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| is_executable(&dir.join(program)))
}

/// Whether `path` is a regular file with an execute bit set.
pub(crate) fn is_executable(path: &Path) -> bool {
    std::fs::metadata(path).is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// Guest text made safe to print on the host terminal: lossy UTF-8 with
/// escape sequences (CSI, OSC, other `ESC x` pairs) and control characters
/// removed, except tab and newline. Guest output is untrusted and must not
/// move the cursor, retitle the window or hide text.
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

/// Read and parse the JSON file `name` in `dir` (at most `cap` bytes).
/// `Ok(None)` when it does not exist.
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

/// Write `value` as pretty JSON to `name` in `dir`, atomically with `mode`.
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
    use super::*;

    fn env(entries: &[&str]) -> Vec<String> {
        entries.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn merge_env_replaces_existing_and_appends_new_keys() {
        let base = env(&["PATH=/bin", "HOME=/root", "TERM=xterm"]);
        let out = merge_env(&base, [("HOME", "/tmp"), ("NEW", "1")], &[]);
        assert_eq!(out, env(&["PATH=/bin", "TERM=xterm", "HOME=/tmp", "NEW=1"]));
    }

    /// A key that is a prefix of another key must not remove it.
    #[test]
    fn merge_env_matches_whole_keys_only() {
        let base = env(&["FOO=1", "FOOBAR=2"]);
        let out = merge_env(&base, [("FOO", "3")], &[]);
        assert_eq!(out, env(&["FOOBAR=2", "FOO=3"]));
    }

    #[test]
    fn merge_env_unset_removes_base_and_override_keys() {
        let base = env(&["A=1", "B=2", "C=3"]);
        let out = merge_env(&base, [("D", "4")], &env(&["B", "D", "MISSING"]));
        assert_eq!(out, env(&["A=1", "C=3"]));
    }

    /// `sh` is on PATH everywhere we run; a nonsense name is not.
    #[test]
    fn on_path_finds_real_programs() {
        assert!(on_path("sh"));
        assert!(!on_path("airlock-definitely-not-a-real-program"));
    }

    /// A directory named like the program must not count as a hit.
    #[test]
    fn on_path_rejects_directories() {
        assert!(!is_executable(Path::new("/")));
    }

    #[test]
    fn strip_controls_removes_escapes_and_controls() {
        assert_eq!(strip_controls(b"plain\ttext\n"), "plain\ttext\n");
        assert_eq!(strip_controls(b"\x1b[1;31mred\x1b[0m"), "red");
        assert_eq!(
            strip_controls(b"\x1b]0;title\x07after \x1b]8;;http://x\x1b\\link"),
            "after link"
        );
        assert_eq!(strip_controls(b"50%\r100%\x08\x00"), "50%100%");
        assert_eq!(strip_controls("\u{9b}c1".as_bytes()), "c1");
        assert_eq!(strip_controls(b"bad \xff utf8"), "bad \u{fffd} utf8");
        // A lone ESC at the end must not panic or swallow earlier text.
        assert_eq!(strip_controls(b"end\x1b"), "end");
    }

    #[test]
    fn json_round_trip_through_a_pinned_dir() {
        let tmp = crate::test_support::TempDir::new("util-json");
        let dir = PinnedDir::open(tmp.path(), Path::new("d"), true).unwrap();
        write_json(&dir, "v.json", &vec![1, 2, 3], 0o600).unwrap();
        let back: Option<Vec<u32>> = read_json(&dir, "v.json", 1024).unwrap();
        assert_eq!(back, Some(vec![1, 2, 3]));
        let missing: Option<Vec<u32>> = read_json(&dir, "none.json", 1024).unwrap();
        assert!(missing.is_none());
        dir.write_atomic("bad.json", b"{", 0o600).unwrap();
        assert!(read_json::<Vec<u32>>(&dir, "bad.json", 1024).is_err());
    }
}
