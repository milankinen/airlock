//! Pack install state.
//!
//! Records which packs are installed on the sandbox disk, and with which
//! definition. The record also tells if the install is confirmed on disk.
//! Pack installation reads this state to decide what to install, and
//! updates it when an install completes.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::plan::Transition;
use crate::util::{self, PinnedDir};

/// File name in the sandbox directory. The file is next to the disk image,
/// thus `airlock rm` deletes both together.
pub const STATE_FILE: &str = "installs.json";
/// Current format version.
const STATE_VERSION: u32 = 1;
/// Maximum size of the state file to read.
const STATE_CAP: u64 = 256 * 1024;

/// Install status of one pack.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub enum PackStatus {
    /// The script exited 0, but the disk sync is not confirmed yet. The
    /// pack stays `unconfirmed` until the install VM confirms its disk sync
    /// at shutdown (see [`promote`]).
    Unconfirmed,
    /// The script exited 0 and the disk sync is confirmed.
    Installed,
    /// The script failed.
    Failed,
    /// The pack was removed from the config, and the user chose to keep it
    /// on the disk. `confirmed` is true if the record was `installed` (not
    /// `unconfirmed`) at that time.
    Kept { confirmed: bool },
}

/// Install record of one pack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Record {
    /// Install status.
    #[serde(flatten)]
    pub status: PackStatus,
    /// [`crate::packs::InstallerScript::fingerprint`] of the install run.
    pub fingerprint: String,
    /// Unix time of the install run.
    pub at: u64,
}

/// Persisted install state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallState {
    /// Format version of the file.
    pub version: u32,
    /// [`crate::project::disk_id`] of the disk that the records describe.
    #[serde(default)]
    pub disk: Option<(u64, u64)>,
    /// Image that the packs were installed on.
    #[serde(default)]
    pub image_id: Option<String>,
    /// Install records by pack name.
    #[serde(default)]
    pub packs: BTreeMap<String, Record>,
    /// True if a normal (non-install) session ran on the disk after the
    /// last install boot. Code that the session left can run in the next
    /// install boot, thus a retry asks first (see
    /// [`super::plan::Why::Retry`]). A file without the field counts as
    /// `true`.
    #[serde(default = "session_unknown")]
    pub ran_session: bool,
}

/// Default of [`InstallState::ran_session`] for a file without the field.
/// Assume that a session ran.
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
    /// Check if the pack `id` has an `installed` record.
    pub fn is_installed(&self, id: &str) -> bool {
        self.packs
            .get(id)
            .is_some_and(|r| r.status == PackStatus::Installed)
    }

    /// Set the record of the pack `id`. The record time is the current
    /// time.
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

/// Result of reading the state file.
#[derive(Debug)]
pub enum ReadState {
    /// No state yet.
    Absent,
    /// Valid state.
    Ok(InstallState),
    /// The file is not readable or not valid. The reason is safe to show.
    Corrupt(String),
    /// Written by a newer airlock.
    TooNew(u32),
}

/// Read and validate the state file.
/// Args:
///  - `dir`: The sandbox directory
///
/// Returns:
///   The state, or why it is not available. A file with an invalid field
///   is `Corrupt`.
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

/// Write `state` to the sandbox directory `dir`. The write is atomic and
/// the file mode is 0600.
pub fn write(dir: &PinnedDir, state: &InstallState) -> anyhow::Result<()> {
    util::write_json(dir, STATE_FILE, state, 0o600)
}

/// Check the fields that the terminal shows or that are compared as IDs.
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

/// Apply record changes to `state`.
/// Args:
///  - `state`: The install state to change
///  - `transitions`: Changes from [`super::plan::decide`] and from the
///    answers to the removed-packs prompt
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

/// Mark `unconfirmed` records as `installed` after a synced shutdown.
/// Args:
///  - `state`: The install state to change
///  - `ids`: The packs whose script exited 0 in that boot
pub fn promote(state: &mut InstallState, ids: &[String]) {
    for (id, r) in &mut state.packs {
        if r.status == PackStatus::Unconfirmed && ids.contains(id) {
            r.status = PackStatus::Installed;
        }
    }
}

/// Current Unix time in seconds.
fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

#[cfg(test)]
mod tests {
    //! Tests of how the install state file is read and how records change.

    use std::path::Path;

    use super::*;
    use crate::test_cfg::temp_dir;

    /// A fingerprint of 64 copies of `c`.
    fn fp(c: char) -> String {
        c.to_string().repeat(64)
    }

    /// A record with `status`.
    fn record(status: PackStatus) -> Record {
        Record {
            status,
            fingerprint: fp('a'),
            at: 3,
        }
    }

    /// Test that the reader refuses a state file that is malformed, was
    /// changed by hand, or comes from a newer airlock. Bad content must not
    /// reach the install plan or the terminal.
    ///   1. Check that a missing file reads as absent
    ///   2. Write bad JSON, bad fields, control characters in names and
    ///      a bad fingerprint, and check that each reads as corrupt
    ///   3. Check that a newer version reads as too new
    ///   4. Check that a kept record without `confirmed` reads as corrupt
    ///   5. Check that a file without `ran_session` reads as if a session
    ///      ran
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
            // A terminal escape sequence in the pack name.
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

    /// Test that keeping a removed pack records if its install was
    /// confirmed, and that only a promote transition changes a kept record.
    ///   1. Keep one installed and one unconfirmed pack
    ///   2. Check that the kept records tell which one was confirmed
    ///   3. Apply a promote transition to the first and check that it is
    ///      installed
    ///   4. Promote the second after a synced shutdown and check that it
    ///      stays kept
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
