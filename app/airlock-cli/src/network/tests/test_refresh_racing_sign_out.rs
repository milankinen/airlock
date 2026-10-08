use serde_json::json;

use crate::services::ServiceId;
use crate::test_cfg::provider::*;

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
