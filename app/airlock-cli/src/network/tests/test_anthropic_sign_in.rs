use serde_json::{Value, json};

use crate::rpc::browser::{BrowserGrant as _, GrantAnswer};
use crate::services::ServiceId;
use crate::services::anthropic::sign_in_pages;
use crate::services::sign_in::LoopbackSignIn;
use crate::services::tokens::TokenKind;
use crate::test_cfg::provider::*;
use crate::test_cfg::services::{free_claude_callback_port, idle_guest, insert_grant};

#[test]
fn code_exchange_with_surrogate_code_gives_guest_surrogates_and_stores_real_tokens() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let access = answer["access_token"].as_str().unwrap();
        let refresh = answer["refresh_token"].as_str().unwrap();
        assert!(access.starts_with("sk-ant-oat01-airlock-"), "{access}");
        assert!(refresh.starts_with("sk-ant-ort01-airlock-"), "{refresh}");
        assert_eq!(answer["expires_in"], 3600);
        assert_eq!(answer["refresh_token_expires_in"], 7_776_000);
        assert_eq!(answer["scope"], CLAUDE_SCOPE);
        assert_eq!(answer["token_type"], "Bearer");
        assert_eq!(answer["token_uuid"], "4f0b8a3e-2c1d-4e5f-9a6b-7c8d9e0f1a2b");
        assert_eq!(
            answer["account"],
            json!({ "uuid": "acct-1", "email_address": "a@example.com" })
        );
        assert_eq!(
            answer["organization"],
            json!({ "uuid": "org-1", "name": "Org" })
        );

        let want = r.exchange_body(CLAUDE_CLIENT_ID, REAL_CODE, r.loopback_redirect());
        let exchange = r.last_seen();
        assert_eq!(exchange.path, "/v1/oauth/token");
        assert_eq!(exchange.body, want.to_string());

        let grant = r
            .store
            .grant_of(ServiceId::Anthropic, access, &[TokenKind::Access])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            grant.real(TokenKind::Access),
            Some("sk-ant-oat01-REAL-ACCESS-0")
        );
        assert_eq!(
            grant.real(TokenKind::Refresh),
            Some("sk-ant-ort01-REAL-REFRESH-0")
        );
        assert_eq!(
            grant.scopes,
            ["org:create_api_key", "user:inference", "user:profile"]
        );
        assert_eq!(grant.client_id, CLAUDE_CLIENT_ID);
        assert_eq!(grant.account.as_deref(), Some("a@example.com"));
        assert_eq!(grant.organization.as_deref(), Some("Org"));
    });
}

#[test]
fn exchange_without_surrogate_code_or_client_id_is_refused_locally() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        for code in [REAL_CODE, "airlock-code-unknown"] {
            let resp = r.exchange(code).await;
            assert_eq!(resp.status, 400, "{code}");
            assert_eq!(resp.json()["error"], "invalid_grant");
        }
        let resp = r
            .exchange_with("", &r.issue_code(), r.loopback_redirect())
            .await;
        assert_eq!(resp.status, 400, "{}", resp.body);
        assert_eq!(resp.json()["error"], "invalid_request");
        assert_eq!(r.upstream_requests(), 0);

        let code = r.issue_code();
        assert_eq!(r.exchange(&code).await.status, 200);
        assert_eq!(r.exchange(&code).await.status, 400);
        assert_eq!(r.upstream_requests(), 1);
    });
}

#[test]
fn manual_exchange_needs_verifier_of_page_opened_for_service() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        let sign_in = LoopbackSignIn::new(ServiceId::Anthropic, sign_in_pages(), r.codes.clone());
        sign_in.attach(&idle_guest());
        let manual = || r.exchange_with(CLAUDE_CLIENT_ID, REAL_CODE, CLAUDE_MANUAL);
        let page = |verifier: &str| {
            claude_sign_in_page(
                CLAUDE_CLIENT_ID,
                "user%3Ainference",
                free_claude_callback_port(),
                verifier,
            )
        };

        let resp = manual().await;
        assert_eq!(resp.status, 400, "{}", resp.body);
        assert_eq!(resp.json()["error"], "invalid_grant");

        let refused = page(VERIFIER)
            .as_str()
            .replace("response_type=code", "response_type=token");
        let answer = sign_in.allow(&url::Url::parse(&refused).unwrap());
        assert!(matches!(answer, GrantAnswer::Refuse(_)), "{answer:?}");
        r.codes
            .open_page(&challenge_of(VERIFIER), ServiceId::Openai);
        assert_eq!(sign_in.allow(&page("w")), GrantAnswer::Allow);
        assert_eq!(manual().await.status, 400);
        assert_eq!(r.upstream_requests(), 0);

        assert_eq!(sign_in.allow(&page(VERIFIER)), GrantAnswer::Allow);
        let resp = manual().await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
        assert_eq!(manual().await.status, 400);
        assert_eq!(r.token_requests(), 1);
        sign_in.detach().await;
    });
}

#[test]
fn console_client_signs_in_refreshes_and_revokes_with_its_client_id() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        let sign_in = LoopbackSignIn::new(ServiceId::Anthropic, sign_in_pages(), r.codes.clone());
        sign_in.attach(&idle_guest());
        let page = claude_sign_in_page(
            CONSOLE_CLIENT_ID,
            "user%3Aprofile+user%3Ainference",
            free_claude_callback_port(),
            VERIFIER,
        );
        assert_eq!(sign_in.allow(&page), GrantAnswer::Allow);
        sign_in.detach().await;

        let resp = r
            .exchange_with(CONSOLE_CLIENT_ID, REAL_CODE, CLAUDE_MANUAL)
            .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
        let refresh = resp.json()["refresh_token"].clone();
        assert_eq!(r.last_body()["client_id"], CONSOLE_CLIENT_ID);
        let grant = r
            .store
            .grant_of(
                ServiceId::Anthropic,
                refresh.as_str().unwrap(),
                &[TokenKind::Refresh],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(grant.client_id, CONSOLE_CLIENT_ID);

        let resp = r
            .post_token(&json!({
                "grant_type": "refresh_token",
                "refresh_token": refresh,
                "client_id": CLAUDE_CLIENT_ID,
            }))
            .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(r.last_body()["client_id"], CONSOLE_CLIENT_ID);

        let resp = r
            .post_revoke(&json!({ "token": refresh, "client_id": "another-client" }))
            .await;
        assert_eq!(resp.status, 200);
        let revokes = r.revokes();
        assert_eq!(revokes.len(), 1);
        assert_eq!(revokes[0]["client_id"], CONSOLE_CLIENT_ID);
        assert_eq!(revokes[0]["token"], "sk-ant-ort01-REAL-REFRESH-1");
        assert!(r.grants().await.is_empty());
    });
}

#[test]
fn exchange_answer_with_credential_in_unknown_place_fails_closed() {
    for (extra, refused) in [
        (
            json!({ "session_token": "unknown-format-secret-0123456789" }),
            true,
        ),
        (
            json!({ "account": { "uuid": "acct-1", "api_key": "unknown-format-key-0123456789" } }),
            true,
        ),
        (
            json!({ "extra": { "key": "sk-ant-api03-REAL-EXTRA" } }),
            true,
        ),
        (json!({ "access_token": "opaque-new-format-token" }), false),
        (json!({ "session_token": "short" }), false),
        (json!({ "refresh_token": 42 }), false),
        (
            json!({ "extra": { "t": ["sk-ant-oat01-REAL-EXTRA"] } }),
            false,
        ),
        (
            json!({
                "access_token": format!("sk-ant-oat01-{}", "A".repeat(95)),
                "refresh_token": format!("sk-ant-ort01-{}", "B".repeat(95)),
            }),
            false,
        ),
    ] {
        let opts = Options {
            exchange_extra: Some(extra.clone()),
            ..Options::default()
        };
        Setup::new(ServiceId::Anthropic, opts).run(|r| async move {
            let resp = r.exchange(&r.issue_code()).await;
            assert!(!resp.body.contains("REAL"), "{extra}: {}", resp.body);
            assert!(!resp.body.contains("AAAA"), "{extra}: {}", resp.body);
            let grants = r.grants().await;
            if refused {
                assert_eq!(resp.status, 502, "{extra}: {}", resp.body);
                assert_eq!(resp.json()["error"], "server_error");
                assert!(grants.is_empty(), "{extra}");
            } else {
                assert_eq!(resp.status, 200, "{extra}: {}", resp.body);
                assert_eq!(grants.len(), 1, "{extra}");
            }
        });
    }
}

#[test]
fn compressed_token_answers_are_refused() {
    let opts = Options {
        gzip: true,
        ..Options::default()
    };
    Setup::new(ServiceId::Anthropic, opts).run(|r| async move {
        let resp = r.exchange(&r.issue_code()).await;
        assert_eq!(resp.status, 502);
        assert_eq!(resp.json()["error"], "server_error");
        assert!(r.grants().await.is_empty());

        insert_grant(
            &r.store,
            ServiceId::Anthropic,
            &[(
                TokenKind::Access,
                &r.fake.access(),
                "sk-ant-oat01-airlock-test",
            )],
        )
        .await;
        let resp = r.create_api_key("sk-ant-oat01-airlock-test").await;
        assert_eq!(resp.status, 502);
        assert!(!resp.body.contains("REAL"));
    });
}

#[test]
fn new_sign_in_of_same_account_replaces_and_revokes_older_grant() {
    for account in [true, false] {
        let opts = Options {
            account,
            ..Options::default()
        };
        Setup::new(ServiceId::Anthropic, opts).run(|r| async move {
            r.sign_in().await;
            r.sign_in().await;
            if account {
                r.wait_for_revokes(1).await;
                let revoked: Vec<Value> = r.revokes().iter().map(|b| b["token"].clone()).collect();
                assert_eq!(revoked, ["sk-ant-ort01-REAL-REFRESH-0"]);
                assert_eq!(r.grants().await.len(), 1);
            } else {
                assert!(r.revokes().is_empty());
                assert_eq!(r.grants().await.len(), 2);
            }
        });
    }
}
