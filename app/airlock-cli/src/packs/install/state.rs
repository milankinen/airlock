//! What is installed on the sandbox disk: `.airlock/sandbox/installs.json`.
//!
//! One record per pack. A pack whose script exited 0 is `unconfirmed`
//! until the install VM confirmed its disk sync at shutdown; only then is
//! it `installed` ([`promote`]). The file lives next to the disk image,
//! so `airlock rm` deletes both together. Every read validates the
//! fields; a file that fails is `Corrupt`.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::plan::Transition;
use crate::util::{self, PinnedDir};

/// File name in the sandbox directory.
pub const STATE_FILE: &str = "installs.json";
/// Current format version.
const STATE_VERSION: u32 = 1;
/// Largest state file read.
const STATE_CAP: u64 = 256 * 1024;

/// The install status of one pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PackStatus {
    /// The script exited 0; the disk sync is not confirmed yet.
    Unconfirmed,
    Installed,
    Failed,
    /// Removed from the config; the user chose to keep it on the disk.
    /// `confirmed`: the record was `installed` (not `unconfirmed`) then.
    Kept {
        confirmed: bool,
    },
}

/// The record of one pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    #[serde(flatten)]
    pub status: PackStatus,
    /// [`crate::packs::InstallerScript::fingerprint`] of the install run.
    pub fingerprint: String,
    /// Unix time of the install run.
    pub at: u64,
}

/// The persisted install state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallState {
    pub version: u32,
    /// [`crate::project::disk_id`] of the disk the records describe.
    #[serde(default)]
    pub disk: Option<(u64, u64)>,
    /// The image the records were installed on.
    #[serde(default)]
    pub image_id: Option<String>,
    #[serde(default)]
    pub packs: BTreeMap<String, Record>,
    /// A normal (non-install) session ran on the disk since the last
    /// install boot: code it left can run in the next install boot, so a
    /// retry asks first ([`super::plan::Why::Retry`]). A file without the
    /// field counts as `true`.
    #[serde(default = "session_unknown")]
    pub ran_session: bool,
}

/// [`InstallState::ran_session`] of a file without the field: assume a
/// session ran.
fn session_unknown() -> bool {
    true
}

impl Default for InstallState {
    fn default() -> Self {
        Self {
            version: STATE_VERSION,
            disk: None,
            image_id: None,
            packs: BTreeMap::new(),
            ran_session: false,
        }
    }
}

impl InstallState {
    /// Whether `id` has an `installed` record.
    pub fn is_installed(&self, id: &str) -> bool {
        self.packs
            .get(id)
            .is_some_and(|r| r.status == PackStatus::Installed)
    }

    /// Set the record of `id`.
    pub fn set(&mut self, id: &str, status: PackStatus, fingerprint: &str) {
        self.packs.insert(
            id.to_string(),
            Record {
                status,
                fingerprint: fingerprint.to_string(),
                at: now_secs(),
            },
        );
    }
}

/// The result of reading the state file.
#[derive(Debug)]
pub enum ReadState {
    /// No state yet.
    Absent,
    Ok(InstallState),
    /// Unreadable or invalid; the reason is safe to show.
    Corrupt(String),
    /// Written by a newer airlock.
    TooNew(u32),
}

/// Read and validate the state in `dir` (the sandbox directory).
pub fn read(dir: &PinnedDir) -> ReadState {
    let value: serde_json::Value = match util::read_json(dir, STATE_FILE, STATE_CAP) {
        Ok(None) => return ReadState::Absent,
        Ok(Some(v)) => v,
        Err(e) => return ReadState::Corrupt(format!("{e:#}")),
    };
    match value.get("version").and_then(serde_json::Value::as_u64) {
        Some(v) if v > u64::from(STATE_VERSION) => {
            return ReadState::TooNew(u32::try_from(v).unwrap_or(u32::MAX));
        }
        Some(0) => return ReadState::Corrupt("version 0".into()),
        Some(_) => {}
        None => return ReadState::Corrupt("no version".into()),
    }
    let state: InstallState = match serde_json::from_value(value) {
        Ok(s) => s,
        Err(e) => return ReadState::Corrupt(e.to_string()),
    };
    match validate(&state) {
        Ok(()) => ReadState::Ok(state),
        Err(e) => ReadState::Corrupt(e),
    }
}

/// Write `state` to `dir` (atomic, 0600).
pub fn write(dir: &PinnedDir, state: &InstallState) -> anyhow::Result<()> {
    util::write_json(dir, STATE_FILE, state, 0o600)
}

/// Reject fields that are shown on the terminal or compared as ids.
fn validate(state: &InstallState) -> Result<(), String> {
    for (id, record) in &state.packs {
        let well_formed = !id.is_empty()
            && id.len() <= 64
            && id
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_');
        if !well_formed {
            return Err(format!("invalid pack name {id:?}"));
        }
        if record.fingerprint.len() != 64
            || !record.fingerprint.chars().all(|c| c.is_ascii_hexdigit())
        {
            return Err(format!("invalid fingerprint of {id}"));
        }
    }
    if let Some(image) = &state.image_id
        && image.chars().any(char::is_control)
    {
        return Err("invalid image id".into());
    }
    Ok(())
}

/// Apply `transitions` (from [`super::plan::decide`] and the answers to
/// the removed-packs prompt) to `state`.
pub fn apply(state: &mut InstallState, transitions: &[Transition]) {
    for t in transitions {
        match t {
            Transition::Reset => {
                state.packs.clear();
                state.disk = None;
                state.image_id = None;
                state.ran_session = false;
            }
            Transition::Promote(id) => {
                if let Some(r) = state.packs.get_mut(id) {
                    r.status = PackStatus::Installed;
                }
            }
            Transition::Drop(id) => {
                state.packs.remove(id);
            }
            Transition::Keep(id) => {
                if let Some(r) = state.packs.get_mut(id) {
                    r.status = PackStatus::Kept {
                        confirmed: r.status == PackStatus::Installed,
                    };
                }
            }
        }
    }
}

/// After a synced shutdown: the `unconfirmed` records of `ids` (the packs
/// that exited 0 in that boot) are `installed`.
pub fn promote(state: &mut InstallState, ids: &[String]) {
    for (id, r) in &mut state.packs {
        if r.status == PackStatus::Unconfirmed && ids.contains(id) {
            r.status = PackStatus::Installed;
        }
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;
    use crate::test_cfg::temp_dir;

    fn fp(c: char) -> String {
        c.to_string().repeat(64)
    }

    fn record(status: PackStatus) -> Record {
        Record {
            status,
            fingerprint: fp('a'),
            at: 3,
        }
    }

    #[test]
    fn reading_malformed_tampered_or_newer_state_file_is_refused() {
        let tmp = temp_dir();
        let dir = PinnedDir::open(tmp.path(), Path::new("sandbox"), true).unwrap();
        assert!(matches!(read(&dir), ReadState::Absent));
        let fingerprint = fp('a');
        let pack = |id: &str, status: &str, fingerprint: &str| {
            format!(
                r#"{{"version": 1, "packs": {{"{id}": {{"status": "{status}", "fingerprint": "{fingerprint}", "at": 1}}}}}}"#
            )
        };
        let corrupt = [
            "{".to_string(),
            "{}".to_string(),
            r#"{"version": 0}"#.to_string(),
            r#"{"version": 1, "packs": 5}"#.to_string(),
            pack("x", "weird", &fingerprint),
            pack("\\u001b[31mx", "installed", &fingerprint),
            pack("x", "installed", "zz"),
            r#"{"version": 1, "image_id": "sha\n"}"#.to_string(),
        ];
        for json in corrupt {
            dir.write_atomic(STATE_FILE, json.as_bytes(), 0o600)
                .unwrap();
            assert!(matches!(read(&dir), ReadState::Corrupt(_)), "{json}");
        }
        dir.write_atomic(STATE_FILE, br#"{"version": 99}"#, 0o600)
            .unwrap();
        assert!(matches!(read(&dir), ReadState::TooNew(99)));
        dir.write_atomic(
            STATE_FILE,
            pack("x", "kept", &fingerprint).as_bytes(),
            0o600,
        )
        .unwrap();
        assert!(matches!(read(&dir), ReadState::Corrupt(_)));
        dir.write_atomic(STATE_FILE, br#"{"version": 1}"#, 0o600)
            .unwrap();
        assert!(matches!(
            read(&dir),
            ReadState::Ok(InstallState {
                ran_session: true,
                ..
            })
        ));
    }

    #[test]
    fn keeping_removed_packs_records_whether_install_was_confirmed() {
        let mut state = InstallState::default();
        state
            .packs
            .insert("a".into(), record(PackStatus::Installed));
        state
            .packs
            .insert("b".into(), record(PackStatus::Unconfirmed));
        apply(
            &mut state,
            &[Transition::Keep("a".into()), Transition::Keep("b".into())],
        );
        assert_eq!(
            state.packs["a"].status,
            PackStatus::Kept { confirmed: true }
        );
        assert_eq!(
            state.packs["b"].status,
            PackStatus::Kept { confirmed: false }
        );
        apply(&mut state, &[Transition::Promote("a".into())]);
        assert!(state.is_installed("a"));
        promote(&mut state, &["b".into()]);
        assert_eq!(
            state.packs["b"].status,
            PackStatus::Kept { confirmed: false }
        );
    }
}
