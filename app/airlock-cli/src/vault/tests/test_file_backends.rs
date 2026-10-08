use super::*;
use crate::test_cfg::temp_dir;
use crate::test_cfg::vault::FixedPassphrase;

#[test]
fn file_vault_keeps_secrets_in_tagged_plaintext_file() {
    let tmp = temp_dir();
    let path = tmp.path().join("vault.json");
    assert!(file_vault(&path).list_secrets().unwrap().is_empty());

    file_vault(&path).set_secret("TOKEN", "abc").unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("\"type\": \"file\""), "{raw}");
    assert_eq!(
        file_vault(&path).get_secret("TOKEN").unwrap().as_deref(),
        Some("abc")
    );
}

#[test]
fn file_vault_handles_merge_each_others_writes() {
    let tmp = temp_dir();
    let path = tmp.path().join("vault.json");
    let a = file_vault(&path);
    let b = file_vault(&path);

    assert!(a.list_secrets().unwrap().is_empty());
    b.set_secret("FROM_B", "1").unwrap();
    a.set_secret("FROM_A", "2").unwrap();

    let fresh = file_vault(&path);
    assert_eq!(fresh.get_secret("FROM_A").unwrap().as_deref(), Some("2"));
    assert_eq!(fresh.get_secret("FROM_B").unwrap().as_deref(), Some("1"));
}

#[test]
fn encrypted_vault_stores_only_ciphertext_and_needs_right_passphrase() {
    let tmp = temp_dir();
    let path = tmp.path().join("vault.json");
    let vault = encrypted_vault(&path, "hunter2");
    vault.set_secret("TOKEN", "abc").unwrap();
    vault.set_secret("OTHER", "xyz").unwrap();

    let raw = std::fs::read_to_string(&path).unwrap();
    assert!(raw.contains("\"type\": \"encrypted-file\""), "{raw}");
    assert!(!raw.contains("abc") && !raw.contains("TOKEN"), "{raw}");
    let reopened = encrypted_vault(&path, "hunter2");
    assert_eq!(
        reopened.get_secret("TOKEN").unwrap().as_deref(),
        Some("abc")
    );
    assert_eq!(
        reopened.get_secret("OTHER").unwrap().as_deref(),
        Some("xyz")
    );
    let err = encrypted_vault(&path, "wrong")
        .get_secret("TOKEN")
        .unwrap_err();
    assert!(err.to_string().contains("wrong passphrase"), "{err:#}");
}

#[test]
fn file_and_encrypted_backends_refuse_each_others_files() {
    let tmp = temp_dir();
    let encrypted = tmp.path().join("encrypted.json");
    let plain = tmp.path().join("plain.json");
    encrypted_vault(&encrypted, "pw")
        .set_secret("TOKEN", "abc")
        .unwrap();
    file_vault(&plain).set_secret("TOKEN", "abc").unwrap();

    let err = file_vault(&encrypted).get_secret("TOKEN").unwrap_err();
    assert!(err.to_string().contains("encrypted vault"), "{err:#}");
    let err = EncryptedFileStorage::new(plain, Box::new(FixedPassphrase("pw")))
        .load()
        .unwrap_err();
    assert!(err.to_string().contains("plaintext vault"), "{err:#}");
}
