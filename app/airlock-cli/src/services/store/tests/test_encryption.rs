//! Encryption of the service token store: what the database holds on
//! disk and which keys can open it.

use crate::services::ServiceId;
use crate::services::store::*;
use crate::test_cfg::services::signed_in_grant;
use crate::test_cfg::{block_on_local, test_db};

const KEY: [u8; 32] = [7; 32];

/// Test that the store encrypts tokens on disk and that only the correct
/// key opens them. A leaked database file must not leak tokens.
///   1. Store a grant with one key
///   2. Read it with a different key and check that decryption fails
///   3. Check that the raw database files do not contain the token
///   4. Check that the account and client ID stay readable
#[test]
fn stored_grant_holds_no_token_in_plain_text_and_opens_with_its_key_only() {
    let (home, db) = test_db();
    block_on_local(async {
        TokenStore::new(db.clone(), &KEY)
            .insert_grant(
                ServiceId::Anthropic,
                signed_in_grant("REALTOKENXYZ", "acct"),
            )
            .await
            .unwrap();
        let other = TokenStore::new(db.clone(), &[8; 32]);
        let err = other
            .snapshot(ServiceId::Anthropic)
            .await
            .err()
            .expect("decryption fails");
        assert!(format!("{err:#}").contains("wrong key"), "{err:#}");
    });
    let mut raw = Vec::new();
    for file in std::fs::read_dir(home.path().join(crate::db::DIR)).unwrap() {
        raw.extend(std::fs::read(file.unwrap().path()).unwrap());
    }
    let raw = String::from_utf8_lossy(&raw);
    assert!(!raw.contains("REALTOKENXYZ"));
    assert!(raw.contains("acct@example.com"));
    assert!(raw.contains("client-1"));
}

/// Test that a sealed value opens only under the same key name and format
/// version. This stops a swap of values between keys and the read of an
/// unknown format.
///   1. Seal a value under one key name and open it again
///   2. Check that a different key name cannot open it
///   3. Change the format version byte and check the error message
#[test]
fn sealed_value_opens_under_its_own_key_name_and_format_version_only() {
    let keys = Keys::derive(&KEY);
    let sealed = keys.seal("anthropic.secrets", b"{}").unwrap();
    assert_eq!(keys.open("anthropic.secrets", &sealed).unwrap(), b"{}");
    assert!(keys.open("openai.secrets", &sealed).is_err());
    let mut other_version = sealed.clone();
    // The first byte of a sealed value is the format version.
    other_version[0] = 2;
    let err = keys.open("anthropic.secrets", &other_version).unwrap_err();
    assert!(err.to_string().contains("format version 2"), "{err}");
}
