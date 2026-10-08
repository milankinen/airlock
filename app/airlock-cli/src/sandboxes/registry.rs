//! Registry of the sandboxes in the data directory.
//!
//! Maps the id of each sandbox in the data directory to its project
//! directory. Sandboxes in their project directory are not in the registry.

use std::ffi::OsStr;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use heed::Database;
use heed::types::{Bytes, Str};

use crate::db::Db;

/// Name of the registry database.
const DATABASE: &str = "sandboxes";

/// Registry table: sandbox id → project path bytes.
type Table = Database<Str, Bytes>;

/// Characters of a sandbox id (lowercase RFC 4648 base32).
const ID_ALPHABET: &[u8; 32] = b"abcdefghijklmnopqrstuvwxyz234567";

/// Number of characters in a sandbox id. Each character holds 5 bits.
const ID_LEN: usize = 8;

/// A registered sandbox.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    /// Sandbox id. It is also the directory name in the data directory.
    pub id: String,
    /// Canonical project directory.
    pub project: PathBuf,
}

/// Check that `id` has the form of a sandbox id. Thus an id can never name
/// a path outside of the sandboxes directory.
pub fn is_valid_id(id: &str) -> bool {
    id.len() == ID_LEN && id.bytes().all(|b| ID_ALPHABET.contains(&b))
}

/// Get all registered sandboxes, ordered by id.
pub async fn list(db: &Db) -> anyhow::Result<Vec<Entry>> {
    let table: Table = db.database(DATABASE).await?;
    db.read(move |txn| {
        let mut entries = Vec::new();
        for item in table.iter(txn)? {
            let (id, project) = item?;
            entries.push(Entry {
                id: id.to_string(),
                project: PathBuf::from(OsStr::from_bytes(project)),
            });
        }
        Ok(entries)
    })
    .await
}

/// Get the project directory of the sandbox `id`.
/// Returns:
///   The project directory, or `None` if `id` is not registered.
pub async fn get(db: &Db, id: &str) -> anyhow::Result<Option<PathBuf>> {
    let table: Table = db.database(DATABASE).await?;
    let id = id.to_string();
    db.read(move |txn| {
        Ok(table
            .get(txn, &id)?
            .map(|project| PathBuf::from(OsStr::from_bytes(project))))
    })
    .await
}

/// Get the id of the sandbox of the project `project`, or register a new
/// sandbox for it. The lookup and the insert are one transaction, so two
/// processes get the same id.
/// Args:
///  - `db`: The airlock database
///  - `boxes_dir`: Sandboxes directory. A new id never names an existing
///    entry in it.
///  - `project`: Canonical project directory
///
/// Returns:
///   The sandbox id.
pub async fn find_or_register(db: &Db, boxes_dir: &Path, project: &Path) -> anyhow::Result<String> {
    let table: Table = db.database(DATABASE).await?;
    let boxes_dir = boxes_dir.to_path_buf();
    let project = project.to_path_buf();
    db.write(move |txn| {
        let key = project.as_os_str().as_bytes();
        for item in table.iter(txn)? {
            let (id, path) = item?;
            if path == key {
                return Ok(id.to_string());
            }
        }
        loop {
            let id = new_id()?;
            let taken = table.get(txn, &id)?.is_some()
                || std::fs::symlink_metadata(boxes_dir.join(&id)).is_ok();
            if !taken {
                table.put(txn, &id, key)?;
                return Ok(id);
            }
        }
    })
    .await
}

/// Remove the sandbox `id` from the registry.
/// Returns:
///   `true` if it was registered.
pub async fn unregister(db: &Db, id: &str) -> anyhow::Result<bool> {
    let table: Table = db.database(DATABASE).await?;
    let id = id.to_string();
    db.write(move |txn| Ok(table.delete(txn, &id)?)).await
}

/// Make a new random sandbox id.
fn new_id() -> anyhow::Result<String> {
    use rand::TryRng;
    let mut bytes = [0u8; 8];
    rand::rngs::SysRng
        .try_fill_bytes(&mut bytes[..5])
        .map_err(|e| anyhow::anyhow!("random sandbox id: {e}"))?;
    let bits = u64::from_be_bytes(bytes) >> 24;
    Ok((0..ID_LEN)
        .rev()
        .map(|i| ID_ALPHABET[((bits >> (i * 5)) & 31) as usize] as char)
        .collect())
}

#[cfg(test)]
mod tests {
    //! Tests for the sandbox registry.

    use super::*;
    use crate::test_cfg::{block_on_local, test_db};

    /// Test that the registry gives one id for each project, that new ids
    /// are valid and skip taken directory names, and that a removed entry is
    /// gone.
    ///   1. Register two projects and check that the ids are valid and differ
    ///   2. Register the first project again and check that it keeps its id
    ///   3. Check the list and the lookup by id
    ///   4. Remove one entry and check that it is gone
    #[test]
    fn register_gives_one_valid_id_per_project() {
        let (home, db) = test_db();
        block_on_local(async {
            let boxes = home.path().join("boxes");
            let a = find_or_register(&db, &boxes, Path::new("/p/a"))
                .await
                .unwrap();
            let b = find_or_register(&db, &boxes, Path::new("/p/b"))
                .await
                .unwrap();
            assert!(is_valid_id(&a) && is_valid_id(&b) && a != b, "{a} {b}");
            assert_eq!(
                find_or_register(&db, &boxes, Path::new("/p/a"))
                    .await
                    .unwrap(),
                a
            );

            let all = list(&db).await.unwrap();
            assert_eq!(all.len(), 2);
            assert_eq!(get(&db, &b).await.unwrap(), Some(PathBuf::from("/p/b")));

            assert!(unregister(&db, &a).await.unwrap());
            assert!(!unregister(&db, &a).await.unwrap());
            assert_eq!(get(&db, &a).await.unwrap(), None);
        });
    }

    /// Test that only ids of the generated form are valid, so that an id
    /// from the command line cannot name a path.
    #[test]
    fn id_check_refuses_paths_and_wrong_lengths() {
        assert!(is_valid_id("abcd2345"));
        for bad in [
            "",
            "abcd234",
            "abcd23456",
            "../abcde",
            "ABCD2345",
            "abcd2341",
        ] {
            assert!(!is_valid_id(bad), "{bad}");
        }
    }
}
