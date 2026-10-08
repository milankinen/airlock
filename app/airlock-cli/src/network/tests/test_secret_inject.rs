//! Tests for secret injection: the proxy swaps surrogates for real values
//! on the way out and masks real values on the way back to the guest.

use std::fmt::Write;

use axum::Router;
use axum::http::HeaderMap;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::project::MaskedSecret;
use crate::test_cfg::network::*;
use crate::test_cfg::upstream::FakeUpstream;
use crate::test_cfg::*;

const REAL: &str = "sk-real-token-0123456789";
const SURROGATE: &str = "SURROGATEabcdef0123456789";
const OTHER_REAL: &str = "sk-other-0123456789";
const OTHER_SURROGATE: &str = "OTHERSURROGATE01234";

/// A masked secret with the given name, real value and surrogate.
fn secret(name: &str, real: &str, surrogate: &str) -> MaskedSecret {
    MaskedSecret {
        name: name.into(),
        real: real.into(),
        surrogate: surrogate.into(),
    }
}

/// A config that injects two secrets on all allowed hosts, with the given
/// middleware scripts.
fn injecting(scripts: Vec<(&'static str, &'static str)>) -> TestNetworkConfig {
    TestNetworkConfig {
        inject: vec![
            secret("TOKEN", REAL, SURROGATE),
            secret("OTHER", OTHER_REAL, OTHER_SURROGATE),
        ],
        middleware_scripts: scripts,
        ..Default::default()
    }
}

/// An upstream that echoes the `authorization`, `x-multi` and `cookie`
/// headers it got in the body. It also copies `authorization` into the
/// `x-echo` response header.
fn echo_app() -> Router {
    Router::new().route(
        "/",
        get(|headers: HeaderMap| async move {
            let auth = headers
                .get("authorization")
                .map_or(String::new(), |v| v.to_str().unwrap().to_string());
            let mut body = String::new();
            for name in ["authorization", "x-multi", "cookie"] {
                for value in headers.get_all(name) {
                    writeln!(body, "{name}: {}", value.to_str().unwrap()).unwrap();
                }
            }
            ([("x-echo", auth)], body)
        }),
    )
}

/// A GET with the surrogate as a Bearer token, plus the `extra` header
/// lines.
fn get_with_auth(port: u16, extra: &str) -> String {
    format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {SURROGATE}\r\n\
         {extra}Connection: close\r\n\r\n"
    )
}

/// Test that the proxy swaps surrogates for real values in request headers
/// and masks real values in response headers. The guest must never see a
/// real value, and the upstream must get it.
///   1. Send headers with surrogates: repeated, with a prefix, in cookies
///   2. Check that the upstream got the real values in each header
///   3. Check that the echoed response header shows the surrogate
#[test]
fn surrogates_in_request_headers_are_swapped_and_real_values_masked_in_response() {
    run_with_config(injecting(vec![]), |proxy, _, _| async move {
        let port = serve(echo_app()).await.port();
        let extra = format!(
            "X-Multi: {SURROGATE}{SURROGATE}\r\nX-Multi: prefix {OTHER_SURROGATE}\r\n\
             Cookie: a={SURROGATE}; b={OTHER_SURROGATE}\r\n"
        );
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&get_with_auth(port, &extra))
            .await;
        // The body shows what the upstream got. Response bodies are not
        // masked, only headers.
        let (head, body) = resp.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            body,
            format!(
                "authorization: Bearer {REAL}\nx-multi: {REAL}{REAL}\n\
                 x-multi: prefix {OTHER_REAL}\ncookie: a={REAL}; b={OTHER_REAL}\n"
            )
        );
        assert!(
            head.contains(&format!("x-echo: Bearer {SURROGATE}")),
            "{head}"
        );
        assert!(!head.contains(REAL), "{head}");
    });
}

/// Test that a middleware sees the real value, and that the proxy masks a
/// real value that the middleware puts in a response header.
///   1. Add a middleware that logs the auth header and copies it into a
///      response header
///   2. Send a request with the surrogate
///   3. Check that the log has the real value
///   4. Check that the response header shows the surrogate
#[test]
fn middleware_sees_real_value_and_its_response_header_is_masked() {
    let cfg = injecting(vec![(
        "copy auth",
        r#"
        local auth = req:header("authorization")
        log(auth)
        local res = req:send()
        res:setHeader("x-from-lua", "leak " .. auth)
        "#,
    )]);
    run_with_config(cfg, |proxy, log, _| async move {
        let port = serve(echo_app()).await.port();
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&get_with_auth(port, ""))
            .await;
        let (head, _) = resp.split_once("\r\n\r\n").unwrap();
        assert_eq!(log.messages(), [format!("Bearer {REAL}")]);
        assert!(
            head.contains(&format!("x-from-lua: leak Bearer {SURROGATE}")),
            "{head}"
        );
        assert!(!head.contains(REAL), "{head}");
    });
}

/// Test that the proxy masks a real value in a middleware error message.
/// The error text goes to the guest in the 502 body.
///   1. Add a middleware that fails with the auth header in its message
///   2. Send a request with the surrogate
///   3. Check the 502 with the surrogate and no real value
#[test]
fn middleware_error_text_with_real_value_is_masked() {
    let cfg = injecting(vec![(
        "fail with header",
        r#"error("bad auth: " .. req:header("authorization"))"#,
    )]);
    run_with_config(cfg, |proxy, _, _| async move {
        let port = serve(echo_app()).await.port();
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&get_with_auth(port, ""))
            .await;
        assert!(resp.starts_with("HTTP/1.1 502"), "{resp}");
        assert!(
            resp.contains(&format!("bad auth: Bearer {SURROGATE}")),
            "{resp}"
        );
        assert!(!resp.contains(REAL), "{resp}");
    });
}

/// Test that a host that only a rule without inject allows gets the
/// surrogate, not the real value.
///   1. Inject the secrets on another host only
///   2. Allow 127.0.0.1 with a rule that does not inject
///   3. Send a request with the surrogate
///   4. Check that the upstream got the surrogate
#[test]
fn host_allowed_without_inject_rule_keeps_surrogate() {
    let cfg = TestNetworkConfig {
        allowed_hosts: vec!["nowhere.example.com".into()],
        plain_allowed_hosts: vec!["127.0.0.1".into()],
        ..injecting(vec![])
    };
    run_with_config(cfg, |proxy, _, _| async move {
        let port = serve(echo_app()).await.port();
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&get_with_auth(port, ""))
            .await;
        let (_, body) = resp.split_once("\r\n\r\n").unwrap();
        assert_eq!(body, format!("authorization: Bearer {SURROGATE}\n"));
    });
}

/// Test that the proxy masks a real value in an HTTPS response header
/// through the TLS MITM.
///   1. Start a TLS upstream that sends the real value in a header
///   2. Send a GET over TLS through the proxy
///   3. Check that the header shows the surrogate and no real value
#[test]
fn https_response_header_with_real_value_is_masked() {
    let upstream = FakeUpstream::bind(&[b"http/1.1"]);
    let cfg = TestNetworkConfig {
        trust_cas: vec![upstream.ca_pem()],
        ..injecting(vec![])
    };
    run_with_config(cfg, |proxy, _, mitm_ca| async move {
        let port = upstream.port();
        upstream.serve(Router::new().route("/", get(|| async { ([("x-secret", REAL)], "ok") })));
        let mut tls = guest_tls(&proxy, &mitm_ca, port, &[b"http/1.1"]).await;
        tls.write_all(http_get(port, "/").as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        let _ = tls.read_to_end(&mut resp).await;
        let resp = String::from_utf8_lossy(&resp);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains(&format!("x-secret: {SURROGATE}")), "{resp}");
        assert!(!resp.contains(REAL), "{resp}");
    });
}
