use std::sync::Arc;

use heed::Database;
use heed::types::{Bytes, Str};

use crate::services::ServiceId;
use crate::services::store::*;
use crate::services::tokens::TokenKind;
use crate::test_cfg::services::signed_in_grant;
use crate::test_cfg::{block_on_local, test_db};

const KEY: [u8; 32] = [7; 32];
const A: ServiceId = ServiceId::Anthropic;

async fn resolve(store: &TokenStore, service: ServiceId, s: &str) -> Option<Resolved> {
    store.resolve(service, s).await.unwrap()
}

#[test]
fn stored_grant_is_found_by_surrogate_of_its_kind_and_service_in_another_process() {
    let (_home, db) = test_db();
    block_on_local(async {
        let a = TokenStore::new(db.clone(), &KEY);
        let grant = a
            .insert_grant(A, signed_in_grant("real-a", "acct"))
            .await
            .unwrap()
            .0;
        let b = TokenStore::new(db.clone(), &KEY);
        let found = resolve(&b, A, "real-a-surrogate").await.unwrap();
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
fn new_sign_in_replaces_grant_of_same_account_client_and_scopes_only() {
    let (_home, db) = test_db();
    block_on_local(async {
        let store = TokenStore::new(db.clone(), &KEY);
        store
            .insert_grant(A, signed_in_grant("first", "acct"))
            .await
            .unwrap();
        store
            .insert_grant(A, signed_in_grant("other", "acct-2"))
            .await
            .unwrap();
        store
            .insert_grant(ServiceId::Openai, signed_in_grant("openai", "acct"))
            .await
            .unwrap();
        let mut narrow = signed_in_grant("narrow", "acct");
        narrow.scopes = vec!["user:inference".into()];
        store.insert_grant(A, narrow).await.unwrap();
        let mut console = signed_in_grant("console", "acct");
        console.client_id = "client-2".into();
        store.insert_grant(A, console).await.unwrap();
        let (_, replaced) = store
            .insert_grant(A, signed_in_grant("second", "acct"))
            .await
            .unwrap();
        let replaced: Vec<Option<&str>> =
            replaced.iter().map(|g| g.real(TokenKind::Access)).collect();
        assert_eq!(replaced, [Some("first")]);

        assert!(resolve(&store, A, "first-surrogate").await.is_none());
        let fresh = TokenStore::new(db.clone(), &KEY);
        assert!(resolve(&fresh, A, "first-surrogate").await.is_none());
        assert!(resolve(&fresh, A, "second-surrogate").await.is_some());
        let mut listed: Vec<(String, String, usize)> = list_grants(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|g| (g.service, g.account.unwrap(), g.scopes.len()))
            .collect();
        listed.sort_unstable();
        let row = |service: &str, account: &str, scopes| (service.into(), account.into(), scopes);
        assert_eq!(
            listed,
            [
                row("anthropic", "acct-2@example.com", 2),
                row("anthropic", "acct@example.com", 1),
                row("anthropic", "acct@example.com", 2),
                row("anthropic", "acct@example.com", 2),
                row("openai", "acct@example.com", 2),
            ]
        );
    });
}

#[test]
fn grant_keeps_newest_api_keys_up_to_limit_in_every_process() {
    let (_home, db) = test_db();
    block_on_local(async {
        let store = TokenStore::new(db.clone(), &KEY);
        let grant = store
            .insert_grant(A, signed_in_grant("t", "acct"))
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
fn write_in_one_process_is_seen_by_other_on_next_lookup() {
    let (_home, db) = test_db();
    block_on_local(async {
        let a = TokenStore::new(db.clone(), &KEY);
        let b = TokenStore::new(db.clone(), &KEY);
        assert_eq!(b.snapshot(A).await.unwrap().generation, 0);
        let grant = a
            .insert_grant(A, signed_in_grant("t", "acct"))
            .await
            .unwrap()
            .0;
        let first = b.snapshot(A).await.unwrap();
        assert_eq!(first.generation, 1);
        assert!(Arc::ptr_eq(&first, &b.snapshot(A).await.unwrap()));
        a.insert_grant(ServiceId::Openai, signed_in_grant("o", "acct"))
            .await
            .unwrap();
        assert!(Arc::ptr_eq(&first, &b.snapshot(A).await.unwrap()));

        a.update_grant(A, &grant.id, |g| {
            g.tokens[0].real = "t2".into();
            Ok(())
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(resolve(&b, A, "t-surrogate").await.unwrap().real, "t2");
        assert_eq!(b.snapshot(A).await.unwrap().generation, 2);

        b.delete_grant(A, &grant.id).await.unwrap();
        for s in [&a, &b] {
            for surrogate in ["t-surrogate", "t-refresh-surrogate"] {
                assert!(resolve(s, A, surrogate).await.is_none(), "{surrogate}");
            }
        }
        assert_eq!(a.snapshot(A).await.unwrap().generation, 3);
        a.delete_grant(A, &grant.id).await.unwrap();
        assert!(
            a.update_grant(A, &grant.id, |_| Ok(()))
                .await
                .unwrap()
                .is_none()
        );
        let services: Vec<String> = list_grants(&db)
            .await
            .unwrap()
            .into_iter()
            .map(|g| g.service)
            .collect();
        assert_eq!(services, ["openai"]);
    });
}

#[test]
fn databases_of_old_layout_are_emptied_on_first_use() {
    let (_home, db) = test_db();
    block_on_local(async {
        for name in OLD_DATABASES {
            let old: Database<Str, Bytes> = db.database(name).await.unwrap();
            db.write(move |txn| {
                old.put(txn, "record", b"sealed grant")?;
                Ok(())
            })
            .await
            .unwrap();
        }
        let store = TokenStore::new(db.clone(), &KEY);
        assert!(store.snapshot(A).await.unwrap().grants.is_empty());
        for name in OLD_DATABASES {
            assert_eq!(db.database_len(name), 0, "{name}");
        }
        assert!(db.has_database(DATABASE));
    });
}

#[test]
fn previous_access_surrogate_stands_for_current_token_until_its_expiry() {
    let (_home, db) = test_db();
    block_on_local(async {
        let store = TokenStore::new(db.clone(), &KEY);
        let grant = store
            .insert_grant(A, signed_in_grant("t", "acct"))
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

#[test]
fn api_key_creations_are_limited_per_grant_across_processes() {
    let (_home, db) = test_db();
    block_on_local(async {
        let a = TokenStore::new(db.clone(), &KEY);
        let grant = a
            .insert_grant(A, signed_in_grant("t", "acct"))
            .await
            .unwrap()
            .0;
        let other = a
            .insert_grant(A, signed_in_grant("u", "acct-2"))
            .await
            .unwrap()
            .0;
        for _ in 0..API_KEY_CREATIONS {
            assert!(a.take_api_key_slot(A, &grant.id).await.unwrap());
        }
        let b = TokenStore::new(db.clone(), &KEY);
        assert!(!b.take_api_key_slot(A, &grant.id).await.unwrap());
        assert!(b.take_api_key_slot(A, &other.id).await.unwrap());
        assert!(!b.take_api_key_slot(A, "no-such-grant").await.unwrap());
    });
}
