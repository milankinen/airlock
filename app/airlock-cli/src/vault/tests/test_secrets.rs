//! Tests for user secrets, registry logins, the service store key and
//! the compatibility of the vault data across versions.

use super::*;
use crate::test_cfg::temp_dir;
use crate::test_cfg::vault::{MemoryStorage, vault_with};

/// The names and masked previews of all secrets in `vault`.
fn previews(vault: &Vault) -> Vec<(String, String)> {
    vault
        .list_secrets()
        .unwrap()
        .into_iter()
        .map(|meta| (meta.name, meta.preview))
        .collect()
}

/// Test that secrets and registry logins written by one vault handle are
/// visible to another handle on the same storage, with masked previews.
///   1. Store, overwrite and add secrets and a registry login in one handle
///   2. Read the secret and the login through a second handle
///   3. Check the previews in name order for each value length
///   4. Remove a secret twice and check that only the first removal finds it
#[test]
fn secrets_and_registry_logins_are_shared_by_vault_handles() {
    let storage = MemoryStorage::default();
    let a = vault_with(storage.clone(), &[]);
    a.set_secret("DATABASE_URL", "value-1").unwrap();
    a.set_secret("DATABASE_URL", "postgres://db").unwrap();
    a.set_secret("_SHORT_1", "abcdefg").unwrap();
    a.set_secret("A1", "password1234567").unwrap();
    a.set_secret("WIDE", &"ñ".repeat(16)).unwrap();
    a.set_registry(
        "ghcr.io",
        &RegistryCreds {
            username: "alice".into(),
            password: "hunter2".into(),
        },
    )
    .unwrap();

    let b = vault_with(storage, &[]);
    assert_eq!(
        b.get_secret("DATABASE_URL").unwrap().as_deref(),
        Some("postgres://db")
    );
    let creds = b.get_registry("ghcr.io").unwrap().unwrap();
    assert_eq!(
        (creds.username.as_str(), creds.password.as_str()),
        ("alice", "hunter2")
    );
    // The preview shows 4 last chars from 16 chars, 2 last chars from 8
    // chars, and no chars below 8. It counts chars, not bytes.
    let s = |v: &str| v.to_string();
    assert_eq!(
        previews(&b),
        [
            (s("A1"), s("****67")),
            (s("DATABASE_URL"), s("****db")),
            (s("WIDE"), s("****ññññ")),
            (s("_SHORT_1"), s("****")),
        ]
    );

    assert!(b.remove_secret("DATABASE_URL").unwrap());
    assert!(!b.remove_secret("DATABASE_URL").unwrap());
    assert!(b.get_secret("DATABASE_URL").unwrap().is_none());
}

/// Test that the vault refuses a secret name that is not a valid env var
/// name and an empty value, because templates refer to secrets as
/// `${NAME}`.
///   1. Store secrets with invalid names and check each error
///   2. Store a secret with an empty value and check the error
///   3. Check that the storage got no write
#[test]
fn secret_that_cannot_be_referenced_as_env_var_is_refused() {
    let storage = MemoryStorage::default();
    let vault = vault_with(storage.clone(), &[]);

    for name in ["", "foo", "1X", "A-B", "A.B", "A B"] {
        assert!(vault.set_secret(name, "x").is_err(), "{name:?}");
    }
    assert!(vault.set_secret("FOO", "").is_err());
    assert!(storage.blob().is_none());
}

/// Test that the service store key is made once, is the same for all
/// handles, and does not show as a user secret.
///   1. Get the key twice through one handle and check that it is the same
///   2. Get the key through a second handle and check that it is the same
///   3. Check that the secret list is empty
#[test]
fn service_store_key_is_created_once_and_is_not_user_secret() {
    let storage = MemoryStorage::default();
    let a = vault_with(storage.clone(), &[]);
    let key = a.service_store_key().unwrap();
    assert_eq!(a.service_store_key().unwrap(), key);

    let b = vault_with(storage, &[]);
    assert_eq!(b.service_store_key().unwrap(), key);
    assert!(b.list_secrets().unwrap().is_empty());
}

/// Test that a write keeps vault fields that this version does not know,
/// so that an older airlock does not erase data of a newer one. The retired
/// `agents` section is the exception and a write removes it.
///   1. Store a blob with an unknown field and an `agents` section
///   2. Write a secret
///   3. Check that the unknown field stays, `agents` is gone and the secret
///      exists
///   4. Do the same for an unknown field in a plaintext vault file
#[test]
fn fields_unknown_to_this_version_survive_write_but_retired_agents_do_not() {
    let storage = MemoryStorage::default();
    storage
        .store(r#"{"secrets":{},"later":{"x":1},"agents":{"claude":{"record":{"version":1}}}}"#)
        .unwrap();
    let vault = vault_with(storage.clone(), &[]);
    vault.set_secret("TOKEN", "abc").unwrap();

    assert_eq!(previews(&vault).len(), 1);
    // The unknown field is not a secret.
    assert!(!vault.remove_secret("later").unwrap());
    let blob: serde_json::Value = serde_json::from_str(&storage.blob().unwrap()).unwrap();
    assert_eq!(blob["later"], serde_json::json!({"x": 1}));
    assert!(blob.get(RETIRED_AGENTS_SECTION).is_none());
    assert!(blob["secrets"]["TOKEN"].is_object());

    let tmp = temp_dir();
    let path = tmp.path().join("vault.json");
    std::fs::write(
        &path,
        r#"{"type":"file","data":{"secrets":{},"future":{"x":1}}}"#,
    )
    .unwrap();
    file_vault(&path).set_secret("TOKEN", "abc").unwrap();
    let raw: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(raw["data"]["future"], serde_json::json!({"x": 1}));
    assert!(raw["data"]["secrets"]["TOKEN"].is_object());
}
