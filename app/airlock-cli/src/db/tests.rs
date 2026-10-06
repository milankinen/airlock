//! The database: creation and modes, transactions through clones, and
//! open errors.

use std::os::unix::fs::PermissionsExt as _;

use heed::types::Str;

use super::*;

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn a_write_is_read_through_a_clone_and_the_files_are_private() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(DIR);
    let db = Db::open(&dir).unwrap();
    rt().block_on(async {
        let table = db.database::<Str, Str>("test.values").await.unwrap();
        db.write(move |txn| Ok(table.put(txn, "key", "value")?))
            .await
            .unwrap();
        let other = db.clone();
        let table = other.database::<Str, Str>("test.values").await.unwrap();
        let value = other
            .read(move |txn| Ok(table.get(txn, "key")?.map(str::to_string)))
            .await
            .unwrap();
        assert_eq!(value.as_deref(), Some("value"));
    });
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("data.mdb")), 0o600);
    assert_eq!(mode(&dir.join("lock.mdb")), 0o600);
}

#[test]
fn a_failed_write_is_not_committed() {
    let home = tempfile::tempdir().unwrap();
    let db = Db::open(&home.path().join(DIR)).unwrap();
    rt().block_on(async {
        let table = db.database::<Str, Str>("test.values").await.unwrap();
        let failed = db
            .write(move |txn| -> anyhow::Result<()> {
                table.put(txn, "key", "value")?;
                bail!("give up")
            })
            .await;
        assert!(failed.is_err());
        let value = db
            .read(move |txn| Ok(table.get(txn, "key")?.map(str::to_string)))
            .await
            .unwrap();
        assert_eq!(value, None);
    });
}

#[test]
fn has_database_does_not_create_one() {
    let home = tempfile::tempdir().unwrap();
    let db = Db::open(&home.path().join(DIR)).unwrap();
    assert!(!db.has_database("test.values"));
    rt().block_on(db.database::<Str, Str>("test.values"))
        .unwrap();
    assert!(db.has_database("test.values"));
}

#[test]
fn a_database_that_cannot_open_is_an_error() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(DIR);
    // A file where the directory should be.
    std::fs::write(&dir, b"").unwrap();
    assert!(Db::open(&dir).is_err());
}

/// LMDB allows one environment handle per path in a process: a second
/// `Db` on the same directory cannot open it.
#[test]
fn a_second_handle_on_one_directory_cannot_open() {
    let home = tempfile::tempdir().unwrap();
    let dir = home.path().join(DIR);
    let _first = Db::open(&dir).unwrap();
    assert!(Db::open(&dir).is_err());
}
