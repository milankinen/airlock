//! A refresh that a sign-out overtakes: the proxy must not keep or leak the
//! new tokens of the provider.

use serde_json::json;

use crate::services::ServiceId;
use crate::test_cfg::provider::*;

/// Test that a refresh that ends after a sign-out revokes its new tokens
/// upstream. Otherwise the provider keeps a live grant that no store holds.
///   1. Sign in, then send a refresh that the provider holds
///   2. While the provider holds it, sign out and check that the grant is gone
///   3. Release the refresh and check that the guest gets no tokens
///   4. Check that the proxy revokes the new real refresh token upstream
#[test]
fn refresh_finishing_after_sign_out_revokes_its_new_tokens_upstream() {
    for service in [ServiceId::Anthropic, ServiceId::Openai] {
        Setup::new(service, Options::default()).run(|r| async move {
            let answer = r.sign_in().await;
            let gate = r.fake.hold_next_refresh();
            let body = json!({
                "grant_type": "refresh_token",
                "refresh_token": answer["refresh_token"],
                "client_id": client_id(service),
            });
            let refresh = r.post_token(&body);
            // The sign-out starts only when the refresh is at the provider.
            // Thus the sign-out always completes in the middle of the refresh.
            let sign_out = async {
                gate.arrived.notified().await;
                let resp = r
                    .post_revoke(&json!({ "token": answer["refresh_token"] }))
                    .await;
                assert_eq!(resp.status, 200);
                assert!(r.grants().await.is_empty());
                gate.release.notify_one();
            };
            let (resp, ()) = tokio::join!(refresh, sign_out);

            assert_ne!(resp.status, 200, "{}", resp.body);
            assert!(!resp.body.contains("REAL"), "{}", resp.body);
            assert_eq!(r.fake.refreshes(), 1);
            let new_refresh = r.fake.refresh();
            r.wait_for_revokes(2).await;
            let revoked: Vec<_> = r.revokes().iter().map(|b| b["token"].clone()).collect();
            assert!(revoked.contains(&json!(new_refresh)), "{revoked:?}");
            assert!(r.grants().await.is_empty());
        });
    }
}
