use crate::services::ServiceId;
use crate::services::store::*;
use crate::test_cfg::services::signed_in_grant;
use crate::test_cfg::{block_on_local, test_db};

const KEY: [u8; 32] = [7; 32];

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

#[test]
fn sealed_value_opens_under_its_own_key_name_and_format_version_only() {
    let keys = Keys::derive(&KEY);
    let sealed = keys.seal("anthropic.secrets", b"{}").unwrap();
    assert_eq!(keys.open("anthropic.secrets", &sealed).unwrap(), b"{}");
    assert!(keys.open("openai.secrets", &sealed).is_err());
    let mut other_version = sealed.clone();
    other_version[0] = 2;
    let err = keys.open("anthropic.secrets", &other_version).unwrap_err();
    assert!(err.to_string().contains("format version 2"), "{err}");
}
