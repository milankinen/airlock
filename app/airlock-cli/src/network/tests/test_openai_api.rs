//! ChatGPT API calls through the proxy: the swap of surrogates on every path
//! and on WebSocket upgrades, and the scan of answers.

use std::sync::Arc;

use crate::services::ServiceId;
use crate::test_cfg::network::TestConnection;
use crate::test_cfg::provider::*;
use crate::test_cfg::tls_trusting;
use crate::test_cfg::upstream::{assert_raw_relay, get_with_bearer, websocket_handshake};

/// Test that the ChatGPT host swaps the surrogate on every path, and that
/// the token host does not serve API paths.
///   1. Sign in
///   2. Send API calls over HTTP/1.1 and HTTP/2 on normal and odd paths
///   3. Check that the provider gets the real token for each one
///   4. Send an API call to the token host and check the local 403
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

/// Test that a WebSocket upgrade request gets the real token. Codex can
/// connect to its responses endpoint with a WebSocket.
///   1. Sign in, with a WebSocket endpoint as the ChatGPT host
///   2. Send an upgrade request with the surrogate over TLS
///   3. Check that the upgrade succeeds
///   4. Check that the endpoint read the real token and not the surrogate
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
        // Put the Codex path and the surrogate into a normal handshake.
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

/// Test that ChatGPT answers with a real token are refused, and that answers
/// with only token-like values pass.
///   1. Sign in
///   2. Get answers with the real access token, the real refresh token, and
///      short token-like identifiers
///   3. Check that the first two become a local 502 and the last passes
///   4. Check that no answer has a real token and the provider got
///      `Accept-Encoding: identity`
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
