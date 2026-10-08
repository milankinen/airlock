use std::sync::mpsc::Sender;
use std::time::Duration;

use super::*;
use crate::test_cfg::temp_dir;
use crate::test_cfg::vault::{MemoryStorage, vault_with};

/// A locked backend whose first write tells `in_store` it holds the vault
/// lock and then stalls, so a writer that skips the lock would read the
/// old blob meanwhile.
struct StallingStorage {
    inner: MemoryStorage,
    in_store: parking_lot::Mutex<Option<Sender<()>>>,
}

impl Storage for StallingStorage {
    fn load(&self) -> anyhow::Result<Option<String>> {
        self.inner.load()
    }
    fn store(&self, data: &str) -> anyhow::Result<()> {
        if let Some(in_store) = self.in_store.lock().take() {
            in_store.send(()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        }
        self.inner.store(data)
    }
    fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
        self.inner.lock_path()
    }
}

struct NoLockPath(MemoryStorage);

impl Storage for NoLockPath {
    fn load(&self) -> anyhow::Result<Option<String>> {
        self.0.load()
    }
    fn store(&self, data: &str) -> anyhow::Result<()> {
        self.0.store(data)
    }
    fn lock_path(&self) -> anyhow::Result<Option<PathBuf>> {
        anyhow::bail!("no home directory")
    }
}

#[test]
fn concurrent_writers_holding_vault_lock_both_survive() {
    let tmp = temp_dir();
    let storage = MemoryStorage::locked(tmp.path().join("vault.keyring.lock"));
    let (in_store, stalled) = std::sync::mpsc::channel();
    let a = vault_with(
        StallingStorage {
            inner: storage.clone(),
            in_store: parking_lot::Mutex::new(Some(in_store)),
        },
        &[],
    );
    let b = vault_with(storage.clone(), &[]);

    let writer_a = std::thread::spawn(move || a.set_secret("A", "1"));
    stalled.recv().unwrap();
    b.set_secret("B", "2").unwrap();
    writer_a.join().unwrap().unwrap();

    let fresh = vault_with(storage, &[]);
    assert_eq!(fresh.get_secret("A").unwrap().as_deref(), Some("1"));
    assert_eq!(fresh.get_secret("B").unwrap().as_deref(), Some("2"));
}

#[test]
fn vault_lock_never_follows_symlink() {
    let tmp = temp_dir();
    let lock = tmp.path().join("vault.keyring.lock");
    let target = tmp.path().join("elsewhere");
    std::os::unix::fs::symlink(&target, &lock).unwrap();
    let vault = vault_with(MemoryStorage::locked(lock), &[]);

    let err = vault.set_secret("A", "1").unwrap_err();

    assert!(format!("{err:#}").contains("open vault lock"), "{err:#}");
    assert!(!target.exists());
    assert!(vault.get_secret("A").unwrap().is_none());
}

#[test]
fn backend_without_lock_path_does_not_write_unlocked() {
    let storage = MemoryStorage::default();
    let vault = vault_with(NoLockPath(storage.clone()), &[]);

    assert!(vault.set_secret("A", "1").is_err());
    assert!(storage.blob().is_none());
}
