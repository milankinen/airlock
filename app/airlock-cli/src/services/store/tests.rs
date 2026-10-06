//! Token store: encryption, the surrogate index, generations across
//! handles. Each handle stands for one airlock process: it has its own
//! cache. LMDB allows one environment handle per path in a process, so
//! the handles share one [`Db`] (as the processes share the files).

use super::*;

const KEY: [u8; 32] = [7; 32];
const A: ServiceId = ServiceId::Anthropic;

/// A database in a fresh temp home (kept alive by the returned guard).
fn test_db() -> (tempfile::TempDir, Db) {
    let home = tempfile::tempdir().unwrap();
    let db = Db::open(&db_dir(&home)).unwrap();
    (home, db)
}

fn db_dir(home: &tempfile::TempDir) -> std::path::PathBuf {
    home.path().join(crate::db::DIR)
}

fn token(kind: TokenKind, real: &str) -> Token {
    Token {
        kind,
        real: real.into(),
        surrogate: format!("{real}-surrogate"),
        expires_at: Some(now_ms() + 60_000),
    }
}

fn new_grant(access: &str, account: &str) -> NewGrant {
    NewGrant {
        account_id: account.into(),
        account: Some(format!("{account}@example.com")),
        organization: Some("Org".into()),
        client_id: "client-1".into(),
        scopes: vec!["user:inference".into(), "user:profile".into()],
        tokens: vec![
            token(TokenKind::Access, access),
            token(TokenKind::Refresh, &format!("{access}-refresh")),
        ],
    }
}

fn rt() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
}

async fn resolve(store: &TokenStore, service: ServiceId, s: &str) -> Option<Resolved> {
    store.resolve(service, s).await.unwrap()
}

#[test]
fn a_grant_round_trips_through_encryption_and_the_index() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let a = TokenStore::new(db.clone(), &KEY);
        let grant = a
            .insert_grant(A, new_grant("real-a", "acct"))
            .await
            .unwrap()
            .0;
        // A second process: nothing cached, everything from the file.
        let b = TokenStore::new(db.clone(), &KEY);
        let found = resolve(&b, A, "real-a-surrogate").await.expect("found");
        assert_eq!(found.grant_id, grant.id);
        assert_eq!(found.kind, TokenKind::Access);
        assert_eq!(found.real, "real-a");
        let stored = b
            .grant_of(A, "real-a-refresh-surrogate", &[TokenKind::Refresh])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(stored.id, grant.id);
        assert_eq!(stored.client_id, "client-1");
        assert_eq!(stored.real(TokenKind::Refresh), Some("real-a-refresh"));

        // Wrong kind, wrong service, unknown surrogate, real token: none.
        assert!(
            b.grant_of(A, "real-a-surrogate", &[TokenKind::Refresh])
                .await
                .unwrap()
                .is_none()
        );
        for (service, s) in [
            (ServiceId::Openai, "real-a-surrogate"),
            (A, "unknown"),
            (A, "real-a"),
        ] {
            assert!(resolve(&b, service, s).await.is_none(), "{service:?} {s}");
        }
    });
}

#[test]
fn the_file_holds_no_token_and_no_surrogate_in_plain_text() {
    let (home, db) = test_db();
    rt().block_on(async {
        let a = TokenStore::new(db.clone(), &KEY);
        a.insert_grant(A, new_grant("REALTOKENXYZ", "acct"))
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
        raw.contains("acct@example.com"),
        "the label is in the plain meta"
    );
    assert!(
        raw.contains("client-1"),
        "the client id is in the plain meta"
    );
}

#[test]
fn a_wrong_key_cannot_decrypt() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        TokenStore::new(db.clone(), &KEY)
            .insert_grant(A, new_grant("real", "acct"))
            .await
            .unwrap();
        let other = TokenStore::new(db.clone(), &[8; 32]);
        let err = other.snapshot(A).await.err().expect("decryption fails");
        assert!(format!("{err:#}").contains("wrong key"), "{err:#}");
    });
    // The AAD binds the sealed value to its key name and format version.
    let keys = Keys::derive(&KEY);
    let sealed = keys.seal("anthropic.secrets", b"{}").unwrap();
    assert_eq!(keys.open("anthropic.secrets", &sealed).unwrap(), b"{}");
    assert!(keys.open("openai.secrets", &sealed).is_err());
    let mut other_version = sealed.clone();
    other_version[0] = 2;
    let err = keys.open("anthropic.secrets", &other_version).unwrap_err();
    assert!(err.to_string().contains("format version 2"), "{err}");
}

#[test]
fn a_new_sign_in_replaces_the_same_account_client_and_scopes() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        store
            .insert_grant(A, new_grant("first", "acct"))
            .await
            .unwrap();
        store
            .insert_grant(A, new_grant("other", "acct-2"))
            .await
            .unwrap();
        let mut narrow = new_grant("narrow", "acct");
        narrow.scopes = vec!["user:inference".into()];
        store.insert_grant(A, narrow).await.unwrap();
        let mut console = new_grant("console", "acct");
        console.client_id = "client-2".into();
        store.insert_grant(A, console).await.unwrap();
        let (_, replaced) = store
            .insert_grant(A, new_grant("second", "acct"))
            .await
            .unwrap();
        let replaced: Vec<Option<&str>> =
            replaced.iter().map(|g| g.real(TokenKind::Access)).collect();
        assert_eq!(replaced, [Some("first")]);

        assert!(resolve(&store, A, "first-surrogate").await.is_none());
        let fresh = TokenStore::new(db.clone(), &KEY);
        assert!(resolve(&fresh, A, "first-surrogate").await.is_none());
        assert!(resolve(&fresh, A, "second-surrogate").await.is_some());
        let listed = list_grants(&db).await.unwrap();
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
                ("acct@example.com", 2),
                ("acct@example.com", 2),
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

/// `airlock show` needs no key: the meta is plain.
#[test]
fn list_grants_reads_the_meta_of_every_service() {
    let (_home, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        store.insert_grant(A, new_grant("a", "acct")).await.unwrap();
        store
            .insert_grant(ServiceId::Openai, new_grant("o", "acct"))
            .await
            .unwrap();
        let services: Vec<String> = list_grants(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|g| g.service)
            .collect();
        assert_eq!(services, ["anthropic", "openai"]);
    });
}

/// A grant keeps the newest keys only; the dropped key's surrogate is no
/// longer found, in this process or another.
#[test]
fn a_grant_keeps_at_most_eight_api_keys() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        let grant = store
            .insert_grant(A, new_grant("t", "acct"))
            .await
            .unwrap()
            .0;
        for n in 0..=MAX_API_KEYS {
            store
                .add_api_keys(
                    A,
                    &grant.id,
                    vec![ApiKey {
                        real: format!("real-{n}"),
                        surrogate: format!("surrogate-{n}"),
                    }],
                )
                .await
                .unwrap();
        }
        for s in [&store, &TokenStore::new(db.clone(), &KEY)] {
            assert!(resolve(s, A, "surrogate-0").await.is_none());
            let kept = resolve(s, A, &format!("surrogate-{MAX_API_KEYS}"))
                .await
                .unwrap();
            assert_eq!(kept.kind, TokenKind::ApiKey);
            assert_eq!(kept.real, format!("real-{MAX_API_KEYS}"));
            let snapshot = s.snapshot(A).await.unwrap();
            assert_eq!(snapshot.grants[&grant.id].api_keys.len(), MAX_API_KEYS);
        }
        let err = store
            .add_api_keys(A, "no-such-grant", vec![])
            .await
            .unwrap_err();
        assert!(err.to_string().contains("removed"), "{err}");
    });
}

#[test]
fn a_deleted_grant_is_gone_everywhere() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        let other = TokenStore::new(db.clone(), &KEY);
        let grant = store
            .insert_grant(A, new_grant("t", "acct"))
            .await
            .unwrap()
            .0;
        // Cached in both handles first.
        assert!(resolve(&store, A, "t-surrogate").await.is_some());
        assert!(resolve(&other, A, "t-surrogate").await.is_some());
        store.delete_grant(A, &grant.id).await.unwrap();
        store.delete_grant(A, &grant.id).await.unwrap();
        for s in [&store, &other] {
            for surrogate in ["t-surrogate", "t-refresh-surrogate"] {
                assert!(resolve(s, A, surrogate).await.is_none(), "{surrogate}");
            }
        }
        assert!(list_grants(&db).await.unwrap().is_empty());
    });
}

/// Two handles (two processes): every write bumps the generation; a
/// reader decrypts again only when the generation changed, and then sees
/// the other's change at once (no time-to-live).
#[test]
fn readers_reload_when_the_generation_changes() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let a = TokenStore::new(db.clone(), &KEY);
        let b = TokenStore::new(db.clone(), &KEY);
        assert_eq!(b.snapshot(A).await.unwrap().generation, 0);
        let grant = a.insert_grant(A, new_grant("t", "acct")).await.unwrap().0;
        let first = b.snapshot(A).await.unwrap();
        assert_eq!(first.generation, 1);
        // No change: the same snapshot, not decrypted again.
        assert!(Arc::ptr_eq(&first, &b.snapshot(A).await.unwrap()));
        // Another service's write leaves this one's generation.
        a.insert_grant(ServiceId::Openai, new_grant("o", "acct"))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first, &b.snapshot(A).await.unwrap()));

        // A refresh in `a` is seen by `b` on its next lookup.
        a.update_grant(A, &grant.id, |g| {
            g.tokens[0].real = "t2".into();
            Ok(())
        })
        .await
        .unwrap()
        .unwrap();
        let found = resolve(&b, A, "t-surrogate").await.unwrap();
        assert_eq!(found.real, "t2");
        assert_eq!(b.snapshot(A).await.unwrap().generation, 2);
        // A sign-out in `b` is seen by `a` on its next lookup.
        b.delete_grant(A, &grant.id).await.unwrap();
        assert!(resolve(&a, A, "t-surrogate").await.is_none());
        assert_eq!(a.snapshot(A).await.unwrap().generation, 3);
        // The update of a deleted grant changes nothing.
        let gone = a.update_grant(A, &grant.id, |_| Ok(())).await.unwrap();
        assert!(gone.is_none());
    });
}

/// The databases of the earlier layout are emptied when a process first
/// uses the store.
#[test]
fn the_old_databases_are_dropped() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        for name in OLD_DATABASES {
            let old: Database<Str, Bytes> = db.database(name).await.unwrap();
            db.write(move |txn| {
                old.put(txn, "record", b"sealed grant")?;
                Ok(())
            })
            .await
            .unwrap();
        }
        for name in OLD_DATABASES {
            assert_eq!(db.database_len(name), 1);
        }
        let store = TokenStore::new(db.clone(), &KEY);
        assert!(store.snapshot(A).await.unwrap().grants.is_empty());
        for name in OLD_DATABASES {
            assert_eq!(db.database_len(name), 0, "{name}");
        }
        assert!(db.has_database(DATABASE));
    });
}

/// A replaced access surrogate stands for the current access token until
/// its own expiry.
#[test]
fn a_previous_access_surrogate_works_until_its_expiry() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let store = TokenStore::new(db.clone(), &KEY);
        let grant = store
            .insert_grant(A, new_grant("t", "acct"))
            .await
            .unwrap()
            .0;
        store
            .update_grant(A, &grant.id, |g| {
                g.keep_previous_access("old-valid".into(), now_ms() + 60_000);
                g.keep_previous_access("old-expired".into(), now_ms() - 1);
                g.tokens[0].real = "t2".into();
                Ok(())
            })
            .await
            .unwrap();
        let found = resolve(&store, A, "old-valid").await.unwrap();
        assert_eq!((found.kind, found.real.as_str()), (TokenKind::Access, "t2"));
        assert!(resolve(&store, A, "old-expired").await.is_none());
    });
}

/// At most three API keys per grant and hour, counted in the store: a
/// second process sees the same count.
#[test]
fn api_key_creations_are_limited_per_grant() {
    let (_dir, db) = test_db();
    rt().block_on(async {
        let a = TokenStore::new(db.clone(), &KEY);
        let grant = a.insert_grant(A, new_grant("t", "acct")).await.unwrap().0;
        let other = a.insert_grant(A, new_grant("u", "acct-2")).await.unwrap().0;
        for _ in 0..API_KEY_CREATIONS {
            assert!(a.take_api_key_slot(A, &grant.id).await.unwrap());
        }
        let b = TokenStore::new(db.clone(), &KEY);
        assert!(!b.take_api_key_slot(A, &grant.id).await.unwrap());
        assert!(b.take_api_key_slot(A, &other.id).await.unwrap());
        assert!(!b.take_api_key_slot(A, "no-such-grant").await.unwrap());
    });
}
