//! Anthropic refreshes and sign-outs through the proxy: what the proxy sends
//! to the provider, what it refuses locally, and how it deletes grants.

use std::time::Duration;

use serde_json::{Value, json};

use crate::services::ServiceId;
use crate::test_cfg::provider::*;
use crate::test_cfg::upstream::post;

/// A Claude Code refresh body with the refresh token `surrogate`.
fn refresh_of(surrogate: &Value) -> Value {
    json!({
        "grant_type": "refresh_token",
        "refresh_token": surrogate,
        "client_id": CLAUDE_CLIENT_ID,
        "scope": "user:inference",
    })
}

/// Test that a relayed refresh sends only the fields that the proxy sets,
/// and that the guest keeps its refresh surrogate. The guest must not
/// control the client or the scopes of the real refresh.
///   1. Sign in with full scopes, or with only `org:create_api_key`
///   2. Send a refresh with a different client, scopes and an extra field
///   3. Check that the provider gets the grant's client and scopes without
///      `org:create_api_key` (no `scope` if none is left)
///   4. Check that the guest gets the same refresh surrogate and that the new
///      access surrogate works with the new real token
#[test]
fn relayed_refresh_sends_only_providers_fields_and_keeps_surrogate() {
    for (scope, sent_scope) in [
        (CLAUDE_SCOPE, Some("user:inference user:profile")),
        ("org:create_api_key", None),
    ] {
        let opts = Options {
            exchange_extra: Some(json!({ "scope": scope })),
            ..Options::default()
        };
        Setup::new(ServiceId::Anthropic, opts).run(|r| async move {
            let answer = r.sign_in().await;
            let resp = r
                .post_token(&json!({
                    "grant_type": "refresh_token",
                    "refresh_token": answer["refresh_token"],
                    "client_id": "another-client",
                    "scope": "org:create_api_key user:inference",
                    "extra": "x",
                }))
                .await;
            assert_eq!(resp.status, 200, "{}", resp.body);
            assert!(!resp.body.contains("REAL"), "{}", resp.body);
            assert_eq!(resp.json()["refresh_token"], answer["refresh_token"]);
            let mut want = json!({
                "grant_type": "refresh_token",
                "refresh_token": "sk-ant-ort01-REAL-REFRESH-0",
                "client_id": CLAUDE_CLIENT_ID,
            });
            if let Some(sent_scope) = sent_scope {
                want["scope"] = sent_scope.into();
            }
            assert_eq!(r.last_body(), want, "{scope}");

            let access = resp.json()["access_token"].as_str().unwrap().to_string();
            assert_eq!(r.api_get("/v1/messages", &access).await.status, 200);
            assert_eq!(
                r.last_seen().header("authorization"),
                Some("Bearer sk-ant-oat01-REAL-ACCESS-1")
            );
        });
    }
}

/// Test that the token endpoint refuses malformed or unknown requests
/// locally. Only a valid request with a known surrogate goes upstream.
///   1. Sign in
///   2. Send token requests with a query, a duplicate key, a wrong content
///      type, a JSON array and unsupported grant types
///   3. Check that each gets a local 400 with the correct error
///   4. Send refreshes with unknown tokens and check `invalid_grant`
///   5. Check that no request goes upstream
#[test]
fn token_requests_that_do_not_fit_are_refused_locally() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let before = r.upstream_requests();
        let refresh = refresh_of(&answer["refresh_token"]).to_string();
        // A second `grant_type` key at the start of the JSON object.
        let duplicate = refresh.replacen('{', r#"{"grant_type":"authorization_code","#, 1);
        for (path, content_type, body, error) in [
            (
                "/v1/oauth/token?x=1",
                "application/json",
                refresh.as_str(),
                "invalid_request",
            ),
            (
                "/v1/oauth/token",
                "application/json",
                &duplicate,
                "invalid_request",
            ),
            ("/v1/oauth/token", "text/plain", &refresh, "invalid_request"),
            (
                "/v1/oauth/token",
                "application/json",
                "[1]",
                "invalid_request",
            ),
            (
                "/v1/oauth/token",
                "application/json",
                r#"{"grant_type":"client_credentials","client_id":"x"}"#,
                "unsupported_grant_type",
            ),
            (
                "/v1/oauth/token",
                "application/x-www-form-urlencoded",
                "grant_type=password&username=u",
                "unsupported_grant_type",
            ),
        ] {
            let resp = r.on_token_host(post(path, content_type, body)).await;
            assert_eq!(resp.status, 400, "{path} {body}");
            assert_eq!(resp.json()["error"], error, "{path} {body}");
        }
        for unknown in ["sk-ant-ort01-unknown", "sk-ant-ort01-airlock-unknown"] {
            let resp = r.post_token(&refresh_of(&unknown.into())).await;
            assert_eq!(resp.status, 400, "{unknown}");
            assert_eq!(resp.json()["error"], "invalid_grant");
        }
        assert_eq!(r.upstream_requests(), before);
    });
}

/// Test that a revoke with the refresh or the access surrogate deletes the
/// grant and revokes the real refresh token upstream.
///   1. Sign in
///   2. Revoke the refresh or the access surrogate
///   3. Check that the provider gets a revoke of the real refresh token and
///      that the grant is gone
///   4. Check that the surrogates now get local refusals
#[test]
fn revoke_of_either_surrogate_deletes_grant_and_revokes_real_refresh_token() {
    for field in ["refresh_token", "access_token"] {
        Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
            let answer = r.sign_in().await;
            let resp = r
                .post_revoke(&json!({
                    "token": answer[field],
                    "token_type_hint": field,
                    "client_id": CLAUDE_CLIENT_ID,
                }))
                .await;
            assert_eq!(resp.status, 200);
            assert_eq!(resp.json(), json!({}));
            assert_eq!(
                r.revokes(),
                [json!({
                    "token": "sk-ant-ort01-REAL-REFRESH-0",
                    "token_type_hint": "refresh_token",
                    "client_id": CLAUDE_CLIENT_ID,
                })],
                "{field}"
            );
            assert!(r.grants().await.is_empty());

            let before = r.upstream_requests();
            let access = answer["access_token"].as_str().unwrap();
            let resp = r.api_get("/v1/messages", access).await;
            assert_eq!(resp.status, 401);
            assert!(resp.body.contains("sign in again"), "{}", resp.body);
            let resp = r.post_token(&refresh_of(&answer["refresh_token"])).await;
            assert_eq!(resp.status, 400);
            assert_eq!(r.upstream_requests(), before);
        });
    }
}

/// Test that a revoke of a token that is not a grant's surrogate stays
/// local. A real token or an API key must not cause an upstream revoke.
///   1. Sign in and create an API key
///   2. Revoke an unknown surrogate, the real refresh token, the API key
///      surrogate and a number
///   3. Check that each gets 200, nothing goes upstream and the grant stays
#[test]
fn revoke_of_unknown_token_or_api_key_stays_local() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let access = answer["access_token"].as_str().unwrap();
        let key = r.create_api_key(access).await.json()["raw_key"].clone();
        let before = r.upstream_requests();
        for token in [
            json!("sk-ant-ort01-airlock-unknown"),
            json!("sk-ant-ort01-REAL-REFRESH-0"),
            key,
            json!(42),
        ] {
            let resp = r.post_revoke(&json!({ "token": token })).await;
            assert_eq!(resp.status, 200, "{token}");
        }
        assert_eq!(r.upstream_requests(), before);
        assert_eq!(r.grants().await.len(), 1);
    });
}

/// Test that a sign-out deletes the grant also when the upstream revoke
/// fails.
///   1. Make the provider answer revokes with 503
///   2. Sign in and revoke the refresh surrogate
///   3. Check that the guest gets 200 and the grant is gone
#[test]
fn failed_upstream_revoke_still_deletes_grant() {
    let opts = Options {
        revoke_status: 503,
        ..Options::default()
    };
    Setup::new(ServiceId::Anthropic, opts).run(|r| async move {
        let answer = r.sign_in().await;
        let resp = r
            .post_revoke(&json!({ "token": answer["refresh_token"] }))
            .await;
        assert_eq!(resp.status, 200);
        assert_eq!(r.revokes().len(), 1);
        assert!(r.grants().await.is_empty());
    });
}

/// Test that a revoke completes also when the guest drops the request.
///   1. Make the provider answer revokes after 300 ms
///   2. Sign in, send a revoke and drop it after 100 ms
///   3. Check that the provider gets the revoke and the grant is gone
#[test]
fn revoke_dropped_by_guest_still_completes() {
    let opts = Options {
        revoke_delay: Duration::from_millis(300),
        ..Options::default()
    };
    Setup::new(ServiceId::Anthropic, opts).run(|r| async move {
        let answer = r.sign_in().await;
        let revoke = json!({ "token": answer["refresh_token"] });
        let dropped =
            tokio::time::timeout(Duration::from_millis(100), r.post_revoke(&revoke)).await;
        assert!(dropped.is_err());
        // Wait longer than the revoke delay, so that the revoke can complete.
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(r.revokes().len(), 1);
        assert!(r.grants().await.is_empty());
    });
}
