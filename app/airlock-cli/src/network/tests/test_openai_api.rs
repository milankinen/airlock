use std::sync::Arc;

use crate::services::ServiceId;
use crate::test_cfg::network::TestConnection;
use crate::test_cfg::provider::*;
use crate::test_cfg::tls_trusting;
use crate::test_cfg::upstream::{assert_raw_relay, get_with_bearer, websocket_handshake};

#[test]
fn chatgpt_requests_on_any_path_carry_real_token_over_h1_and_h2() {
    Setup::new(ServiceId::Openai, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let access = answer["access_token"].as_str().unwrap();
        let real = format!("Bearer {}", r.fake.access());
        for (h2, path) in [
            (false, "/backend-api/codex/models"),
            (true, "/backend-api/codex/models"),
            (false, "/backend-apix/codex"),
            (false, "/"),
            (false, "//Backend-API/codex/models"),
            (false, "/backend-api/%2e%2e/x"),
            (true, "/backend-api/codex/"),
        ] {
            let resp = r
                .request(r.api_port, h2, get_with_bearer(path, access))
                .await;
            assert_eq!(resp.status, 200, "h2={h2} {path}: {}", resp.body);
            assert_eq!(
                r.last_seen().header("authorization"),
                Some(real.as_str()),
                "{path}"
            );
        }
        let before = r.upstream_requests();
        let resp = r
            .on_token_host(get_with_bearer("/backend-api/codex/models", access))
            .await;
        assert_eq!(resp.status, 403, "{}", resp.body);
        assert_eq!(r.upstream_requests(), before);
    });
}

#[test]
fn websocket_upgrade_carries_real_token() {
    let opts = Options {
        api: ApiHost::WebSocket,
        ..Options::default()
    };
    Setup::new(ServiceId::Openai, opts).run(|r| async move {
        let answer = r.sign_in().await;
        let access = answer["access_token"].as_str().unwrap();
        let conn = TestConnection::connect(&r.proxy, "127.0.0.1", r.api_port)
            .await
            .unwrap();
        let mut stream = tokio_rustls::TlsConnector::from(Arc::new(tls_trusting(&r.mitm)))
            .connect(
                rustls::pki_types::ServerName::try_from("127.0.0.1").unwrap(),
                conn.into_stream(),
            )
            .await
            .unwrap();
        let handshake = websocket_handshake(r.api_port, true)
            .replacen("GET /ws ", "GET /backend-api/codex/responses ", 1)
            .replacen(
                "\r\n",
                &format!("\r\nAuthorization: Bearer {access}\r\n"),
                1,
            );
        assert_raw_relay(&mut stream, &handshake, "HTTP/1.1 101").await;

        let read = String::from_utf8_lossy(&r.ws_read.lock().unwrap()).to_lowercase();
        let real = format!("authorization: bearer {}", r.fake.access()).to_lowercase();
        assert!(read.contains(&real), "{read}");
        assert!(!read.contains(&access.to_lowercase()), "{read}");
    });
}

#[test]
fn chatgpt_answers_with_real_token_are_refused() {
    Setup::new(ServiceId::Openai, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let access = answer["access_token"].as_str().unwrap();
        for (path, refused) in [
            ("/backend-api/leak/access", true),
            ("/backend-api/leak/refresh", true),
            ("/backend-api/identifiers", false),
        ] {
            let mut req = get_with_bearer(path, access);
            req.headers_mut()
                .insert("accept-encoding", "gzip".parse().unwrap());
            let resp = r.on_api(req).await;
            assert_eq!(resp.status == 502, refused, "{path}: {}", resp.body);
            assert!(!resp.body.contains("REAL"), "{path}: {}", resp.body);
            assert_eq!(r.last_seen().header("accept-encoding"), Some("identity"));
        }
    });
}
