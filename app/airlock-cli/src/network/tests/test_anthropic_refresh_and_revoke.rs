use std::time::Duration;

use serde_json::{Value, json};

use crate::services::ServiceId;
use crate::test_cfg::provider::*;
use crate::test_cfg::upstream::post;

fn refresh_of(surrogate: &Value) -> Value {
    json!({
        "grant_type": "refresh_token",
        "refresh_token": surrogate,
        "client_id": CLAUDE_CLIENT_ID,
        "scope": "user:inference",
    })
}

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

#[test]
fn token_requests_that_do_not_fit_are_refused_locally() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let before = r.upstream_requests();
        let refresh = refresh_of(&answer["refresh_token"]).to_string();
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
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(r.revokes().len(), 1);
        assert!(r.grants().await.is_empty());
    });
}
