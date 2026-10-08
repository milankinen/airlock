//! Tests for the plaintext and encrypted file backends of the vault.

use super::*;
use crate::test_cfg::temp_dir;
use crate::test_cfg::vault::FixedPassphrase;

/// Test that the plaintext file vault writes its secrets to a file with
/// the `file` type tag and reads them back in a new handle.
///   1. Check that a vault with no file has no secrets
///   2. Store a secret
///   3. Check the type tag in the file
///   4. Read the secret through a new handle
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

/// Test that two handles of one file vault do not erase the writes of each
/// other, because each write reads the latest file first.
///   1. Open two handles and load the data in the first handle
///   2. Write a secret through the second handle, then through the first
///   3. Check that a new handle sees both secrets
#[test]
fn two_file_vault_handles_keep_writes_of_each_other() {
    let tmp = temp_dir();
    let path = tmp.path().join("vault.json");
    let a = file_vault(&path);
    let b = file_vault(&path);

    // Load the data in `a` now, so its cached copy is old after `b` writes.
    assert!(a.list_secrets().unwrap().is_empty());
    b.set_secret("FROM_B", "1").unwrap();
    a.set_secret("FROM_A", "2").unwrap();

    let fresh = file_vault(&path);
    assert_eq!(fresh.get_secret("FROM_A").unwrap().as_deref(), Some("2"));
    assert_eq!(fresh.get_secret("FROM_B").unwrap().as_deref(), Some("1"));
}

/// Test that the encrypted vault writes no secret names or values in clear
/// text, and that only the correct passphrase opens it.
///   1. Store two secrets in an encrypted vault
///   2. Check the type tag and that the file has no secret name or value
///   3. Read both secrets with the same passphrase
///   4. Check that a wrong passphrase gives an error
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

/// Test that each file backend refuses a file of the other type, so that
/// a wrong configuration does not overwrite a vault.
///   1. Write an encrypted vault file and a plaintext vault file
///   2. Read the encrypted file with the plaintext backend and check the
///      error
///   3. Read the plaintext file with the encrypted backend and check the
///      error
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
