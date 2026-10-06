//! Token store: encryption, lookups, and the grant cache across handles.
//! Each handle stands for one airlock process: it has its own cache.
//! LMDB allows one environment handle per path in a process, so the
//! handles share one [`Db`] (as the processes share the files).

use std::time::Duration;

use super::*;

const KEY: [u8; 32] = [7; 32];

/// A database in a fresh temp home (kept alive by the returned guard).
fn test_db() -> (tempfile::TempDir, Db) {
    let home = tempfile::tempdir().unwrap();
    let db = Db::open(&db_dir(&home)).unwrap();
    (home, db)
}

fn db_dir(home: &tempfile::TempDir) -> std::path::PathBuf {
    home.path().join(crate::db::DIR)
}

fn secrets(access: &str) -> GrantSecrets {
    GrantSecrets {
        access_token: access.into(),
        access_expires_at: now_ms() + 60_000,
        refresh_token: Some(format!("{access}-refresh")),
        id_token: None,
        scopes: vec!["user:inference".into(), "user:profile".into()],
        surrogates: Surrogates {
            access: format!("{access}-surrogate"),
            previous_access: vec![],
            refresh: Some(format!("{access}-refresh-surrogate")),
            id_token: None,
        },
        api_keys: vec![],
    }
}

fn new_grant(access: &str, account: &str) -> NewGrant {
    NewGrant {
        service: ServiceId::Anthropic,
        account_id: account.into(),
        account_label: Some(format!("{account}@example.com")),
        secrets: secrets(access),
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

#[test]
fn a_grant_round_trips_through_encryption_and_lookup() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let a = TokenStore::new(db.clone(), &KEY);
        let grant = a.insert_grant(new_grant("real-a", "acct")).await.unwrap().0;
        // A second process: nothing cached, everything from the file.
        let b = TokenStore::new(db.clone(), &KEY);
        let found = b
            .find_by_surrogate(
                ServiceId::Anthropic,
                SurrogateKind::Access,
                "real-a-surrogate",
            )
            .await
            .unwrap()
            .expect("found by the access surrogate");
        assert_eq!(found.id, grant.id);
        assert_eq!(found.secrets.access_token, "real-a");
        assert_eq!(
            found.secrets.refresh_token.as_deref(),
            Some("real-a-refresh")
        );
        let by_refresh = b
            .find_by_surrogate(
                ServiceId::Anthropic,
                SurrogateKind::Refresh,
                "real-a-refresh-surrogate",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(by_refresh.id, grant.id);

        // Wrong kind, wrong service, unknown surrogate, real token: none.
        for (service, kind, s) in [
            (
                ServiceId::Anthropic,
                SurrogateKind::Refresh,
                "real-a-surrogate",
            ),
            (ServiceId::Openai, SurrogateKind::Access, "real-a-surrogate"),
            (ServiceId::Anthropic, SurrogateKind::Access, "unknown"),
            (ServiceId::Anthropic, SurrogateKind::Access, "real-a"),
        ] {
            assert!(
                b.find_by_surrogate(service, kind, s)
                    .await
                    .unwrap()
                    .is_none(),
                "{service:?} {kind:?} {s}"
            );
        }
    });
}

#[test]
fn the_file_holds_no_token_and_no_surrogate_in_plain_text() {
    let (home, db) = test_db();
    rt().block_on(async {
        let a = TokenStore::new(db.clone(), &KEY);
        a.insert_grant(new_grant("REALTOKENXYZ", "acct"))
            .await
            .unwrap();
    });
    let mut raw = Vec::new();
    for file in std::fs::read_dir(db_dir(&home)).unwrap() {
        raw.extend(std::fs::read(file.unwrap().path()).unwrap());
    }
    let raw = String::from_utf8_lossy(&raw);
    assert!(!raw.contains("REALTOKENXYZ"), "a token is in plain text");
    assert!(
        !raw.contains("REALTOKENXYZ-surrogate"),
        "a surrogate is in plain text"
    );
    assert!(
        raw.contains("acct@example.com"),
        "the label is a plain field"
    );
}

#[test]
fn a_wrong_key_finds_nothing_and_cannot_decrypt() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let grant = TokenStore::new(db.clone(), &KEY)
            .insert_grant(new_grant("real", "acct"))
            .await
            .unwrap()
            .0;
        let other = TokenStore::new(db.clone(), &[8; 32]);
        assert!(
            other
                .find_by_surrogate(
                    ServiceId::Anthropic,
                    SurrogateKind::Access,
                    "real-surrogate"
                )
                .await
                .unwrap()
                .is_none()
        );
        // Even with the record at hand, the other key fails the AEAD tag.
        let (tables, keys) = other.tables().await.unwrap();
        let id = grant.id.clone();
        let decrypted = other
            .db
            .read(move |txn| Ok(tables.grant(txn, &keys, &id, ServiceId::Anthropic)))
            .await
            .unwrap();
        let Err(err) = decrypted else {
            panic!("decryption fails");
        };
        assert!(err.to_string().contains("wrong key"), "{err}");
    });
    // The AAD binds the ciphertext to its grant id and service.
    let keys = Keys::derive(&KEY);
    let sealed = keys
        .seal("id-1", ServiceId::Anthropic, &secrets("x"))
        .unwrap();
    assert!(
        keys.open(
            "id-1",
            ServiceId::Anthropic,
            &sealed.nonce,
            &sealed.ciphertext
        )
        .is_ok()
    );
    assert!(
        keys.open(
            "id-2",
            ServiceId::Anthropic,
            &sealed.nonce,
            &sealed.ciphertext
        )
        .is_err()
    );
    assert!(
        keys.open("id-1", ServiceId::Openai, &sealed.nonce, &sealed.ciphertext)
            .is_err()
    );
}

#[test]
fn a_new_sign_in_replaces_the_same_account_and_scopes() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        store
            .insert_grant(new_grant("first", "acct"))
            .await
            .unwrap();
        store
            .insert_grant(new_grant("other", "acct-2"))
            .await
            .unwrap();
        let mut narrow = new_grant("narrow", "acct");
        narrow.secrets.scopes = vec!["user:inference".into()];
        store.insert_grant(narrow).await.unwrap();
        store
            .insert_grant(new_grant("second", "acct"))
            .await
            .unwrap();

        let gone = store
            .find_by_surrogate(
                ServiceId::Anthropic,
                SurrogateKind::Access,
                "first-surrogate",
            )
            .await
            .unwrap();
        assert!(
            gone.is_none(),
            "the replaced grant is gone (also from the cache)"
        );
        let fresh = TokenStore::new(db.clone(), &KEY);
        assert!(
            fresh
                .find_by_surrogate(
                    ServiceId::Anthropic,
                    SurrogateKind::Access,
                    "first-surrogate"
                )
                .await
                .unwrap()
                .is_none()
        );
        let listed = list_grants(&db).await.unwrap();
        // Sorted: grants created in the same millisecond list in any order.
        let mut accounts: Vec<(&str, usize)> = listed
            .iter()
            .map(|g| (g.account.as_deref().unwrap(), g.scopes.len()))
            .collect();
        accounts.sort_unstable();
        assert_eq!(
            accounts,
            [
                ("acct-2@example.com", 2),
                ("acct@example.com", 1),
                ("acct@example.com", 2)
            ]
        );
        assert!(listed.iter().all(|g| g.service == "anthropic"));
    });
}

#[test]
fn list_grants_of_an_empty_store_is_empty() {
    let (_home, db) = test_db();
    rt().block_on(async {
        assert!(list_grants(&db).await.unwrap().is_empty());
    });
}

#[test]
fn api_keys_are_found_by_their_surrogate() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        let grant = store.insert_grant(new_grant("t", "acct")).await.unwrap().0;
        store
            .add_api_key(
                &grant.id,
                ServiceId::Anthropic,
                ApiKey {
                    real: "sk-ant-api03-real".into(),
                    surrogate: "sk-ant-api03-surrogate".into(),
                },
            )
            .await
            .unwrap();
        let other = TokenStore::new(db.clone(), &KEY);
        let found = other
            .find_by_surrogate(
                ServiceId::Anthropic,
                SurrogateKind::ApiKey,
                "sk-ant-api03-surrogate",
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(found.secrets.api_keys[0].real, "sk-ant-api03-real");
        assert_eq!(found.version, 2);
    });
}

/// A grant keeps the newest keys only; the dropped key's surrogate is no
/// longer found, in this process or another.
#[test]
fn a_grant_keeps_at_most_eight_api_keys() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        let grant = store.insert_grant(new_grant("t", "acct")).await.unwrap().0;
        for n in 0..=MAX_API_KEYS {
            store
                .add_api_key(
                    &grant.id,
                    ServiceId::Anthropic,
                    ApiKey {
                        real: format!("real-{n}"),
                        surrogate: format!("surrogate-{n}"),
                    },
                )
                .await
                .unwrap();
        }
        for s in [&store, &TokenStore::new(db.clone(), &KEY)] {
            let find = |n: usize| {
                let surrogate = format!("surrogate-{n}");
                async move {
                    s.find_by_surrogate(ServiceId::Anthropic, SurrogateKind::ApiKey, &surrogate)
                        .await
                        .unwrap()
                }
            };
            assert!(find(0).await.is_none());
            let kept = find(MAX_API_KEYS).await.unwrap();
            assert_eq!(kept.secrets.api_keys.len(), MAX_API_KEYS);
            assert_eq!(kept.secrets.api_keys[0].real, "real-1");
        }
    });
}

#[test]
fn a_deleted_grant_is_gone_with_its_lookups() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        let grant = store.insert_grant(new_grant("t", "acct")).await.unwrap().0;
        // Cached in this handle first.
        assert!(
            store
                .find_by_surrogate(ServiceId::Anthropic, SurrogateKind::Access, "t-surrogate")
                .await
                .unwrap()
                .is_some()
        );
        store.delete_grant(&grant.id).await.unwrap();
        store.delete_grant(&grant.id).await.unwrap();
        for s in [&store, &TokenStore::new(db.clone(), &KEY)] {
            for (kind, surrogate) in [
                (SurrogateKind::Access, "t-surrogate"),
                (SurrogateKind::Refresh, "t-refresh-surrogate"),
            ] {
                assert!(
                    s.find_by_surrogate(ServiceId::Anthropic, kind, surrogate)
                        .await
                        .unwrap()
                        .is_none()
                );
            }
        }
        assert!(list_grants(&db).await.unwrap().is_empty());
    });
}

/// A new sign-in hands back the secrets of the grants it replaced, so
/// they can be revoked upstream.
#[test]
fn a_replaced_grant_hands_back_its_secrets() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        let (_, replaced) = store
            .insert_grant(new_grant("first", "acct"))
            .await
            .unwrap();
        assert!(replaced.is_empty());
        let (_, replaced) = store
            .insert_grant(new_grant("second", "acct"))
            .await
            .unwrap();
        let tokens: Vec<_> = replaced
            .iter()
            .map(|s| (s.access_token.as_str(), s.refresh_token.as_deref()))
            .collect();
        assert_eq!(tokens, [("first", Some("first-refresh"))]);
    });
}

/// Another process deletes a grant: this process stops using its cached
/// copy once the cache time is over.
#[test]
fn a_grant_deleted_elsewhere_leaves_the_cache_in_time() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let ttl = Duration::from_millis(100);
        let a = TokenStore::new(db.clone(), &KEY).with_cache_ttl(ttl);
        let grant = a.insert_grant(new_grant("t", "acct")).await.unwrap().0;
        TokenStore::new(db.clone(), &KEY)
            .delete_grant(&grant.id)
            .await
            .unwrap();
        // Within the cache time the copy is still used.
        let find =
            || a.find_by_surrogate(ServiceId::Anthropic, SurrogateKind::Access, "t-surrogate");
        assert!(find().await.unwrap().is_some());
        tokio::time::sleep(ttl).await;
        assert!(find().await.unwrap().is_none());
        assert!(a.cache.lock().grants.is_empty());
    });
}

/// At most three API keys per grant and hour, counted in the record: a
/// second process sees the same count.
#[test]
fn api_key_creations_are_limited_per_grant() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let a = TokenStore::new(db.clone(), &KEY);
        let grant = a.insert_grant(new_grant("t", "acct")).await.unwrap().0;
        let other = a.insert_grant(new_grant("u", "acct-2")).await.unwrap().0;
        for _ in 0..API_KEY_CREATIONS {
            assert!(a.take_api_key_slot(&grant.id).await.unwrap());
        }
        let b = TokenStore::new(db.clone(), &KEY);
        assert!(!b.take_api_key_slot(&grant.id).await.unwrap());
        assert!(b.take_api_key_slot(&other.id).await.unwrap());
        assert!(!b.take_api_key_slot("no-such-grant").await.unwrap());
    });
}
