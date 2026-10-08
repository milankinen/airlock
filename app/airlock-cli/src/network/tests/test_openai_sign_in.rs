//! OpenAI (Codex) sign-ins, refreshes and sign-outs through the proxy: the
//! fake JWTs for the guest, the device-code sign-in, and the local refusals.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};

use crate::services::ServiceId;
use crate::services::auth_codes::Channel;
use crate::services::tokens::{FAKE_JWT_PREFIX, TokenKind};
use crate::test_cfg::provider::*;
use crate::test_cfg::upstream::post;

/// Provider options with access tokens that expire after 60 seconds.
fn short_lived() -> Options {
    Options {
        expires_in: 60,
        ..Options::default()
    }
}

/// A Codex refresh body with the refresh token `surrogate`.
fn refresh_of(surrogate: &Value) -> Value {
    json!({ "grant_type": "refresh_token", "refresh_token": surrogate })
}

/// Test that a code exchange with a surrogate code gives the guest fake JWTs
/// and stores the real tokens. Codex reads claims from the JWTs, so the fake
/// JWTs must carry the claims of the real ones.
///   1. Sign in with a surrogate code
///   2. Check that the ID and access tokens are unsigned JWTs with the real
///      claims and a nonce
///   3. Check that the provider got the exchange with the real code
///   4. Check that the store has the real tokens, the client and the account
#[test]
fn code_exchange_with_surrogate_code_gives_guest_fake_jwts_and_stores_real_tokens() {
    Setup::new(ServiceId::Openai, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let id_token = answer["id_token"].as_str().unwrap();
        let access = answer["access_token"].as_str().unwrap();
        let refresh = answer["refresh_token"].as_str().unwrap();
        assert!(refresh.starts_with("airlock-rt-"), "{refresh}");
        for (token, real) in [
            (id_token, FakeProvider::id_token("plus")),
            (access, r.fake.access()),
        ] {
            let parts: Vec<&str> = token.split('.').collect();
            assert_eq!(parts.len(), 3, "{token}");
            assert!(!token.contains('='), "{token}");
            let header: Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(parts[0]).unwrap()).unwrap();
            assert_eq!(header, json!({ "alg": "none", "typ": "JWT" }));
            let claims = claims_of(token);
            assert_eq!(claims["exp"], claims_of(&real)["exp"]);
            assert!(claims["airlock_nonce"].is_string());
        }
        let id_claims = claims_of(id_token);
        assert_eq!(id_claims["email"], "o@example.com");
        assert_eq!(
            id_claims["https://api.openai.com/auth"]["chatgpt_account_id"],
            "acc-1"
        );
        assert_eq!(
            claims_of(access)["https://api.openai.com/auth"]["chatgpt_plan_type"],
            "plus"
        );

        let sent = form_fields(&r.last_seen().body);
        let want = r.exchange_body(CODEX_CLIENT_ID, REAL_CODE, r.loopback_redirect());
        assert_eq!(serde_json::to_value(sent).unwrap(), want);

        let grant = r
            .store
            .grant_of(ServiceId::Openai, access, &[TokenKind::Access])
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            grant.real(TokenKind::Access),
            Some(r.fake.access().as_str())
        );
        assert_eq!(
            grant.real(TokenKind::Refresh),
            Some(r.fake.refresh().as_str())
        );
        assert_eq!(
            grant.real(TokenKind::Id),
            Some(FakeProvider::id_token("plus").as_str())
        );
        assert_eq!(grant.client_id, CODEX_CLIENT_ID);
        let listed = r.grants().await;
        assert_eq!(listed[0].account.as_deref(), Some("o@example.com"));
        assert_eq!(listed[0].service, "openai");
    });
}

/// Test that an exchange is refused locally without a surrogate code of this
/// service.
///   1. Exchange the real code and a surrogate code of Anthropic
///   2. Check that each gets `invalid_grant` and nothing goes upstream
#[test]
fn exchange_without_surrogate_code_of_service_is_refused_locally() {
    Setup::new(ServiceId::Openai, Options::default()).run(|r| async move {
        let anthropic_code = r
            .codes
            .issue(REAL_CODE, ServiceId::Anthropic, Channel::Callback(1455))
            .unwrap();
        for code in [REAL_CODE, anthropic_code.as_str()] {
            let resp = r.exchange(code).await;
            assert_eq!(resp.status, 400, "{code}: {}", resp.body);
            assert_eq!(resp.json()["error"], "invalid_grant");
        }
        assert_eq!(r.upstream_requests(), 0);
    });
}

/// Test that the device-code poll gives the guest a surrogate code that
/// works only in the device flow.
///   1. Poll for the device code and check that the guest gets a surrogate
///   2. Exchange it on the loopback callback and check `invalid_grant`
///   3. Exchange it on the device callback and check that it is used up
///   4. Exchange a fresh device surrogate code and check that the provider
///      gets the real device code
#[test]
fn device_code_reaches_guest_as_surrogate_bound_to_device_flow() {
    Setup::new(ServiceId::Openai, Options::default()).run(|r| async move {
        let poll = json!({ "device_auth_id": "d", "user_code": "U" });
        let resp = r
            .on_token_host(post(
                "/api/accounts/deviceauth/token",
                "application/json",
                &poll.to_string(),
            ))
            .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains(REAL_DEVICE_CODE), "{}", resp.body);
        assert_eq!(resp.json()["code_verifier"], VERIFIER);
        let code = resp.json()["authorization_code"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(code.starts_with("airlock-code-"), "{code}");

        // The wrong channel uses up the code too.
        let resp = r.exchange(&code).await;
        assert_eq!(resp.status, 400, "{}", resp.body);
        assert_eq!(resp.json()["error"], "invalid_grant");
        assert_eq!(r.last_seen().path, "/api/accounts/deviceauth/token");
        let resp = r.exchange_with(CODEX_CLIENT_ID, &code, CODEX_DEVICE).await;
        assert_eq!(resp.status, 400, "used up: {}", resp.body);

        let code = r
            .codes
            .issue(REAL_DEVICE_CODE, ServiceId::Openai, Channel::Device)
            .unwrap();
        let resp = r.exchange_with(CODEX_CLIENT_ID, &code, CODEX_DEVICE).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(form_fields(&r.last_seen().body)["code"], REAL_DEVICE_CODE);
    });
}

/// Test that tokens that are not JWTs get opaque surrogates that work for
/// API calls, refreshes and revokes.
///   1. Make the provider issue opaque tokens and sign in
///   2. Check the opaque surrogates and that an API call gets the real token
///   3. Refresh and check that the provider gets the real refresh token
///   4. Revoke and check that the provider gets the new real refresh token
#[test]
fn opaque_tokens_get_opaque_surrogates_that_work_upstream() {
    Setup::new(ServiceId::Openai, short_lived()).run(|r| async move {
        r.fake.set_access("REAL-opaque-access-token");
        r.fake.set_refresh("REAL-short");
        let answer = r.sign_in().await;
        let access = answer["access_token"].as_str().unwrap();
        let refresh = answer["refresh_token"].as_str().unwrap();
        assert!(access.starts_with("airlock-at-"), "{access}");
        assert!(refresh.starts_with("airlock-rt-"), "{refresh}");

        let resp = r.api_get("/backend-api/codex/models", access).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(
            r.last_seen().header("authorization"),
            Some("Bearer REAL-opaque-access-token")
        );

        let resp = r.post_token(&refresh_of(&answer["refresh_token"])).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
        assert_eq!(r.last_body()["refresh_token"], "REAL-short");
        assert_eq!(resp.json()["refresh_token"], refresh);

        let resp = r.post_revoke(&json!({ "token": refresh })).await;
        assert_eq!(resp.status, 200);
        assert_eq!(r.revokes()[0]["token"], r.fake.refresh());
    });
}

/// Test that a relayed refresh sends only the fields that the proxy sets,
/// and that the guest gets the new claims. Codex reads the plan from the
/// new ID token.
///   1. Sign in and make the provider issue an ID token with a new plan
///   2. Refresh with a different client, a scope and an extra field
///   3. Check that the provider gets only the grant's client and the real
///      refresh token
///   4. Check that the guest keeps its refresh surrogate and gets the new
///      plan
#[test]
fn relayed_refresh_sends_only_providers_fields_and_carries_new_claims() {
    Setup::new(ServiceId::Openai, short_lived()).run(|r| async move {
        let answer = r.sign_in().await;
        let real_refresh = r.fake.refresh();
        *r.fake.refreshed_plan.lock().unwrap() = Some("pro".into());
        let resp = r
            .post_token(&json!({
                "client_id": "another-client",
                "grant_type": "refresh_token",
                "refresh_token": answer["refresh_token"],
                "scope": "openid api.everything",
                "extra": "x",
            }))
            .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
        assert_eq!(r.fake.refreshes(), 1);
        assert_eq!(
            r.last_body(),
            json!({
                "client_id": CODEX_CLIENT_ID,
                "grant_type": "refresh_token",
                "refresh_token": real_refresh,
            })
        );
        let refreshed = resp.json();
        assert_eq!(refreshed["refresh_token"], answer["refresh_token"]);
        let claims = claims_of(refreshed["id_token"].as_str().unwrap());
        assert_eq!(
            claims["https://api.openai.com/auth"]["chatgpt_plan_type"],
            "pro"
        );
    });
}

/// Test that a refresh answer is swapped completely or refused completely.
/// A refused answer must not change the stored tokens.
///   1. Sign in and add a real JWT to the refresh answer
///   2. Refresh and check that the guest gets a fake JWT for it
///   3. Add an unknown secret to the refresh answer and refresh
///   4. Check the 502 and that the store keeps the real refresh token of
///      the last good refresh
#[test]
fn refresh_answer_is_swapped_or_refused_whole() {
    Setup::new(ServiceId::Openai, short_lived()).run(|r| async move {
        let answer = r.sign_in().await;
        let refresh = refresh_of(&answer["refresh_token"]);
        *r.fake.refresh_extra.lock().unwrap() =
            Some(json!({ "session": FakeProvider::id_token("plus") }));
        let resp = r.post_token(&refresh).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(!resp.body.contains("REAL"), "{}", resp.body);
        let session = resp.json()["session"].as_str().unwrap().to_string();
        assert!(session.starts_with(FAKE_JWT_PREFIX), "{session}");
        let stored = r.fake.refresh();

        *r.fake.refresh_extra.lock().unwrap() =
            Some(json!({ "session_token": "opaque-secret-value-0123456789" }));
        let resp = r.post_token(&refresh).await;
        assert_eq!(resp.status, 502, "{}", resp.body);
        let grant = r
            .store
            .grant_of(
                ServiceId::Openai,
                answer["refresh_token"].as_str().unwrap(),
                &[TokenKind::Refresh],
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(grant.real(TokenKind::Refresh), Some(stored.as_str()));
    });
}

/// Test that a token exchange grant and an unknown refresh token are
/// refused locally. A token exchange gets an API key for the ID token. The
/// proxy never forwards it, and Codex accepts the refusal.
///   1. Sign in
///   2. Send a token exchange for an API key and check
///      `unsupported_grant_type`
///   3. Refresh with an unknown surrogate and check the 401
///      `refresh_token_invalidated`
///   4. Check that nothing goes upstream
#[test]
fn token_exchange_grant_and_unknown_refresh_are_refused_locally() {
    Setup::new(ServiceId::Openai, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let before = r.upstream_requests();
        let exchange = url::form_urlencoded::Serializer::new(String::new())
            .append_pair(
                "grant_type",
                "urn:ietf:params:oauth:grant-type:token-exchange",
            )
            .append_pair("client_id", CODEX_CLIENT_ID)
            .append_pair("requested_token", "openai-api-key")
            .append_pair("subject_token", answer["id_token"].as_str().unwrap())
            .append_pair(
                "subject_token_type",
                "urn:ietf:params:oauth:token-type:id_token",
            )
            .finish();
        let resp = r
            .on_token_host(post(
                "/oauth/token",
                "application/x-www-form-urlencoded",
                &exchange,
            ))
            .await;
        assert_eq!(resp.status, 400);
        assert_eq!(resp.json()["error"], "unsupported_grant_type");

        let resp = r.post_token(&refresh_of(&"airlock-rt-nope".into())).await;
        assert_eq!(resp.status, 401);
        assert_eq!(resp.json()["error"], "refresh_token_invalidated");
        assert_eq!(r.upstream_requests(), before);
    });
}

/// Test that a sign-out deletes the grant and revokes both real tokens
/// upstream.
///   1. Sign in and revoke the refresh surrogate
///   2. Check that the provider gets revokes of the real refresh and access
///      tokens and that the grant is gone
///   3. Revoke again and check that nothing goes upstream
#[test]
fn revoke_deletes_grant_and_revokes_both_real_tokens() {
    Setup::new(ServiceId::Openai, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let revoke = json!({
            "token": answer["refresh_token"],
            "token_type_hint": "refresh_token",
            "client_id": CODEX_CLIENT_ID,
        });
        let resp = r.post_revoke(&revoke).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.json(), json!({}));
        assert_eq!(
            r.revokes(),
            [
                json!({
                    "token": r.fake.refresh(),
                    "token_type_hint": "refresh_token",
                    "client_id": CODEX_CLIENT_ID,
                }),
                json!({
                    "token": r.fake.access(),
                    "token_type_hint": "access_token",
                }),
            ]
        );
        assert!(r.grants().await.is_empty());

        let before = r.upstream_requests();
        assert_eq!(r.post_revoke(&revoke).await.status, 200);
        assert_eq!(r.upstream_requests(), before);
    });
}
