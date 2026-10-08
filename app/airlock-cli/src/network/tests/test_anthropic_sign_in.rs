//! Anthropic sign-ins through the proxy: the code exchange, the manual and
//! Console sign-ins, and the refusal of exchange answers that the proxy
//! cannot make safe.

use serde_json::{Value, json};

use crate::rpc::browser::{BrowserGrant as _, GrantAnswer};
use crate::services::ServiceId;
use crate::services::anthropic::sign_in_pages;
use crate::services::sign_in::LoopbackSignIn;
use crate::services::tokens::TokenKind;
use crate::test_cfg::provider::*;
use crate::test_cfg::services::{free_claude_callback_port, idle_guest, insert_grant};

/// Test that a code exchange with a surrogate code gives the guest
/// surrogates and stores the real tokens. The guest must get all other
/// fields of the answer unchanged, because Claude Code reads them.
///   1. Sign in with a surrogate code
///   2. Check the surrogate tokens and the other fields of the answer
///   3. Check that the provider got the exchange with the real code
///   4. Check that the store has the real tokens, scopes, client, account
///      and organization
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

/// Test that an exchange without a surrogate code or a client id is refused
/// locally, and that a surrogate code works only once.
///   1. Exchange the real code and an unknown surrogate code, and check
///      `invalid_grant`
///   2. Exchange a surrogate code without a client id and check
///      `invalid_request`
///   3. Exchange a surrogate code two times and check that only the first
///      one goes upstream
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

/// Test that a manual exchange with the real code goes upstream only if
/// its PKCE verifier belongs to a sign-in page that the browser bridge
/// opened for this service. Otherwise the guest can bring any real code.
///   1. Send a manual exchange before any page opens and check
///      `invalid_grant`
///   2. Open a page with a wrong response type, a page of the other service
///      and a page of a different verifier, and check that the exchange is
///      still refused
///   3. Open the page of the correct verifier
///   4. Check that one exchange succeeds and a second one is refused
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
        // The correct verifier, but of a page that OpenAI opened.
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

/// Test that a Console sign-in keeps the Console client id for the
/// exchange, the refresh and the revoke. The provider binds the tokens to
/// that client, whatever client id the guest sends later.
///   1. Open the Console sign-in page and exchange the code
///   2. Check that the grant has the Console client id
///   3. Refresh with the Claude client id and check that the provider gets
///      the Console client id
///   4. Revoke with a different client id and check the same, and that the
///      grant is gone
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

/// Test that an exchange answer with a credential in an unknown place fails
/// closed. The proxy cannot swap a credential that it does not find, so it
/// must refuse the whole answer.
///   1. Add extra fields to the exchange answer of the provider
///   2. Sign in
///   3. Check that answers with an unknown secret under a token-like key,
///      or with an API key, become a 502 and store no grant
///   4. Check that the other answers succeed
///   5. Check that no answer shows a real token to the guest
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
        // These pass: a new access token format in its usual field, a value
        // too short for a credential, a number, an access token in an array
        // (the proxy swaps it), and tokens of the full real length.
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

/// Test that compressed token answers are refused. The proxy cannot read
/// them to swap the real tokens.
///   1. Make the provider compress its answers
///   2. Sign in and check the 502 and that no grant is stored
///   3. Create an API key with a stored grant and check the 502
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

/// Test that a new sign-in of the same account replaces the older grant and
/// revokes its real refresh token. Without an account in the answer, the
/// proxy cannot know the account and keeps both grants.
///   1. Sign in two times, with or without an account in the answer
///   2. With an account, check one revoke of the first token and one grant
///   3. Without an account, check no revoke and two grants
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
