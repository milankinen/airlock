//! Install snapshots.
//!
//! Keeps a copy of the definition of each pack on the sandbox disk: the
//! files, the version, the args and a keyed hash of each setup value. When
//! a pack changes, airlock shows what changed since the install. The copies
//! are next to the install records, and only the copies of packs that have
//! an install record stay.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use super::state::InstallState;
use crate::packs::snapshot::PackSnapshot;
use crate::util::{self, PinnedDir};

/// File name in the sandbox directory, next to the install records.
pub const SNAPSHOTS_FILE: &str = "pack-snapshots.json";
/// Maximum size of the file to read and write.
// The files of one pack are 1 MiB or less. In JSON, they can be 6 MiB.
// If the file is too large, the write removes the file texts. Thus the
// read always accepts a file that airlock wrote.
pub const SNAPSHOTS_CAP: u64 = 32 * 1024 * 1024;
/// Format version of the file.
const SNAPSHOTS_VERSION: u32 = 1;

/// The install snapshots of a sandbox.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallSnapshots {
    /// Format version (see [`SNAPSHOTS_VERSION`]).
    version: u32,
    /// The copies by install fingerprint (see
    /// [`crate::packs::InstallerScript::fingerprint`]).
    packs: BTreeMap<String, PackSnapshot>,
}

impl InstallSnapshots {
    /// Get the copy of the install with the fingerprint `fingerprint`.
    pub fn get(&self, fingerprint: &str) -> Option<&PackSnapshot> {
        self.packs.get(fingerprint)
    }

    /// Add the copy `snapshot` of the install with the fingerprint
    /// `fingerprint`. It replaces an older copy.
    pub fn insert(&mut self, fingerprint: &str, snapshot: PackSnapshot) {
        self.packs.insert(fingerprint.to_string(), snapshot);
    }

    /// Give the copy of the install `old` also to the fingerprint `new`.
    /// The disk keeps the old install, but the record has the new
    /// fingerprint (see [`super::plan::Transition::Accept`]).
    pub fn rename(&mut self, old: &str, new: &str) {
        if let Some(snapshot) = self.packs.get(old).cloned() {
            self.packs.insert(new.to_string(), snapshot);
        }
    }

    /// Return the copies of the installs that have a record in `state`.
    fn pruned(&self, state: &InstallState) -> Self {
        let packs = self
            .packs
            .iter()
            .filter(|(fingerprint, _)| {
                state
                    .packs
                    .values()
                    .any(|record| &record.fingerprint == *fingerprint)
            })
            .map(|(fingerprint, snapshot)| (fingerprint.clone(), snapshot.clone()))
            .collect();
        Self {
            version: SNAPSHOTS_VERSION,
            packs,
        }
    }
}

/// Read the install snapshots of the sandbox directory `dir`.
/// Returns:
///   The snapshots. No snapshots if the file does not exist, is too large,
///   is not valid or comes from a newer airlock. Then the changed-pack
///   question cannot show the changes.
pub fn read(dir: &PinnedDir) -> InstallSnapshots {
    match util::read_json::<InstallSnapshots>(dir, SNAPSHOTS_FILE, SNAPSHOTS_CAP) {
        Ok(Some(snapshots)) if snapshots.version == SNAPSHOTS_VERSION => snapshots,
        Ok(Some(snapshots)) => {
            tracing::warn!("{SNAPSHOTS_FILE}: version {}", snapshots.version);
            InstallSnapshots::default()
        }
        Ok(None) => InstallSnapshots::default(),
        Err(e) => {
            tracing::warn!("{SNAPSHOTS_FILE}: {e:#}");
            InstallSnapshots::default()
        }
    }
}

/// Write the copies of `snapshots` that have a record in `state` to the
/// sandbox directory `dir`. The write is atomic and the file mode is 0600.
/// If the file is too large, the copies keep only the file hashes.
pub fn write(
    dir: &PinnedDir,
    snapshots: &InstallSnapshots,
    state: &InstallState,
) -> anyhow::Result<()> {
    let mut pruned = snapshots.pruned(state);
    if serde_json::to_vec_pretty(&pruned)?.len() as u64 >= SNAPSHOTS_CAP {
        pruned.packs.values_mut().for_each(PackSnapshot::drop_texts);
    }
    util::write_json(dir, SNAPSHOTS_FILE, &pruned, 0o600)
}

#[cfg(test)]
mod tests {
    //! Tests of the install snapshot file.

    use std::path::Path;

    use super::*;
    use crate::packs::install::state::PackStatus;
    use crate::test_cfg::temp_dir;

    /// A fingerprint of 64 copies of `c`.
    fn fp(c: char) -> String {
        c.to_string().repeat(64)
    }

    /// Test that the file keeps only the copies of installs that have a
    /// record, and that a renamed copy stays for the new fingerprint. Old
    /// copies must not grow the file.
    ///   1. Add copies for the fingerprints a, b and c
    ///   2. Give the copy of b also to d (a kept old install)
    ///   3. Write with records for a and d, and read the file again
    ///   4. Check that only a and d stay, and that d has the copy of b
    ///   5. Check that a corrupt file reads as empty
    #[test]
    fn write_prunes_snapshots_to_install_records() {
        let tmp = temp_dir();
        let dir = PinnedDir::open(tmp.path(), Path::new("sandbox"), true).unwrap();
        let mut snapshots = InstallSnapshots::default();
        for (c, text) in [('a', "a\n"), ('b', "b\n"), ('c', "c\n")] {
            let snapshot = PackSnapshot::new("~p", [("setup.sh", text.as_bytes())]);
            snapshots.insert(&fp(c), snapshot);
        }
        snapshots.rename(&fp('b'), &fp('d'));
        let mut state = InstallState::default();
        state.set("~p", PackStatus::Installed, &fp('a'));
        state.set("~q", PackStatus::Installed, &fp('d'));
        write(&dir, &snapshots, &state).unwrap();

        let read_back = read(&dir);
        assert_eq!(
            read_back.packs.keys().collect::<Vec<_>>(),
            [&fp('a'), &fp('d')]
        );
        assert_eq!(read_back.get(&fp('d')), snapshots.get(&fp('b')));

        dir.write_atomic(SNAPSHOTS_FILE, b"{", 0o600).unwrap();
        assert_eq!(read(&dir), InstallSnapshots::default());
    }
}
