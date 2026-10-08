use std::os::unix::fs::PermissionsExt as _;

use heed::types::Str;

use crate::db::{DIR, Db};
use crate::test_cfg::{block_on_local, temp_dir};

fn mode(path: &std::path::Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

#[test]
fn database_commits_successful_writes_for_all_clones_in_private_files() {
    let home = temp_dir();
    let dir = home.path().join(DIR);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    let db = Db::open(&dir).unwrap();
    assert!(!db.has_database("test.values"));

    block_on_local(async {
        let table = db.database::<Str, Str>("test.values").await.unwrap();
        db.write(move |txn| Ok(table.put(txn, "kept", "value")?))
            .await
            .unwrap();
        let failed = db
            .write(move |txn| -> anyhow::Result<()> {
                table.put(txn, "dropped", "value")?;
                anyhow::bail!("give up")
            })
            .await;
        assert!(failed.is_err());

        let other = db.clone();
        let table = other.database::<Str, Str>("test.values").await.unwrap();
        let values = other
            .read(move |txn| {
                Ok((
                    table.get(txn, "kept")?.map(str::to_string),
                    table.get(txn, "dropped")?.map(str::to_string),
                ))
            })
            .await
            .unwrap();
        assert_eq!(values, (Some("value".to_string()), None));
    });

    assert!(db.has_database("test.values"));
    assert_eq!(mode(&dir), 0o700);
    assert_eq!(mode(&dir.join("data.mdb")), 0o600);
    assert_eq!(mode(&dir.join("lock.mdb")), 0o600);
}

#[test]
fn database_that_cannot_open_is_error() {
    let home = temp_dir();
    let file = home.path().join("file");
    std::fs::write(&file, b"").unwrap();
    assert!(Db::open(&file).is_err());

    let dir = home.path().join(DIR);
    let _first = Db::open(&dir).unwrap();
    assert!(Db::open(&dir).is_err());
}
