//! Tests for secret injection: the proxy swaps surrogates for real values
//! on the way out and masks real values on the way back to the guest.

use std::fmt::Write;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::Router;
use axum::body::Body;
use axum::http::HeaderMap;
use axum::routing::get;
use futures::StreamExt as _;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::project::MaskedSecret;
use crate::test_cfg::network::*;
use crate::test_cfg::upstream::FakeUpstream;
use crate::test_cfg::*;

// Each surrogate has the length of its real value, as a real surrogate has.
const REAL: &str = "sk-real-token-0123456789";
const SURROGATE: &str = "SURROGATEabcdef012345678";
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
/// Returns:
///   The upstream, and the text of its last body as it sent it. The proxy
///   masks the body that the guest gets, so only this text shows what the
///   upstream got.
fn echo_app() -> (Router, Arc<Mutex<String>>) {
    let sent = Arc::new(Mutex::new(String::new()));
    let last = sent.clone();
    let app = Router::new().route(
        "/",
        get(move |headers: HeaderMap| async move {
            let auth = headers
                .get("authorization")
                .map_or(String::new(), |v| v.to_str().unwrap().to_string());
            let mut body = String::new();
            for name in ["authorization", "x-multi", "cookie"] {
                for value in headers.get_all(name) {
                    writeln!(body, "{name}: {}", value.to_str().unwrap()).unwrap();
                }
            }
            last.lock().unwrap().clone_from(&body);
            ([("x-echo", auth)], body)
        }),
    );
    (app, sent)
}

/// Decode a chunked HTTP/1.1 `body`.
fn dechunk(mut body: &str) -> String {
    let mut out = String::new();
    loop {
        let (size, rest) = body.split_once("\r\n").unwrap();
        let size = usize::from_str_radix(size, 16).unwrap();
        if size == 0 {
            return out;
        }
        out.push_str(&rest[..size]);
        body = &rest[size + 2..];
    }
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
/// and masks real values in the response headers and body. The guest must
/// never see a real value, and the upstream must get it.
///   1. Send headers with surrogates: repeated, with a prefix, in cookies
///   2. Check that the upstream got the real values in each header
///   3. Check that the echoed response header and body show the surrogates
#[test]
fn surrogates_in_request_headers_are_swapped_and_real_values_masked_in_response() {
    run_with_config(injecting(vec![]), |proxy, _, _| async move {
        let (app, upstream_sent) = echo_app();
        let port = serve(app).await.port();
        let extra = format!(
            "X-Multi: {SURROGATE}{SURROGATE}\r\nX-Multi: prefix {OTHER_SURROGATE}\r\n\
             Cookie: a={SURROGATE}; b={OTHER_SURROGATE}\r\n"
        );
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&get_with_auth(port, &extra))
            .await;
        let (head, body) = resp.split_once("\r\n\r\n").unwrap();
        let echo = |real: &str, other: &str| {
            format!(
                "authorization: Bearer {real}\nx-multi: {real}{real}\n\
                 x-multi: prefix {other}\ncookie: a={real}; b={other}\n"
            )
        };
        assert_eq!(*upstream_sent.lock().unwrap(), echo(REAL, OTHER_REAL));
        assert_eq!(body, echo(SURROGATE, OTHER_SURROGATE));
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
        let port = serve(echo_app().0).await.port();
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
        let port = serve(echo_app().0).await.port();
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
/// surrogate, not the real value. The body shows what the upstream got,
/// because the proxy masks nothing for this host.
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
        let port = serve(echo_app().0).await.port();
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&get_with_auth(port, ""))
            .await;
        let (_, body) = resp.split_once("\r\n\r\n").unwrap();
        assert_eq!(body, format!("authorization: Bearer {SURROGATE}\n"));
    });
}

/// Test that the proxy masks a real value in an HTTPS response header and
/// body through the TLS MITM.
///   1. Start a TLS upstream that sends the real value in a header and in
///      the body
///   2. Send a GET over TLS through the proxy
///   3. Check that the header and the body show the surrogate and no real
///      value
#[test]
fn https_response_header_and_body_with_real_value_are_masked() {
    let upstream = FakeUpstream::bind(&[b"http/1.1"]);
    let cfg = TestNetworkConfig {
        trust_cas: vec![upstream.ca_pem()],
        ..injecting(vec![])
    };
    run_with_config(cfg, |proxy, _, mitm_ca| async move {
        let port = upstream.port();
        upstream.serve(Router::new().route(
            "/",
            get(|| async { ([("x-secret", REAL)], format!("ok {REAL}")) }),
        ));
        let mut tls = guest_tls(&proxy, &mitm_ca, port, &[b"http/1.1"]).await;
        tls.write_all(http_get(port, "/").as_bytes()).await.unwrap();
        let mut resp = Vec::new();
        let _ = tls.read_to_end(&mut resp).await;
        let resp = String::from_utf8_lossy(&resp);
        assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
        assert!(resp.contains(&format!("x-secret: {SURROGATE}")), "{resp}");
        assert!(resp.ends_with(&format!("ok {SURROGATE}")), "{resp}");
        assert!(!resp.contains(REAL), "{resp}");
    });
}

/// Test that the proxy masks a real value that the upstream splits across
/// two body chunks, and that it asks the upstream for an uncompressed body.
/// A streamed answer can split a value at any byte.
///   1. Start an upstream that streams the real values in chunks with a
///      pause, split in the middle of each value
///   2. Send a request that accepts gzip
///   3. Check that the upstream got `accept-encoding: identity`
///   4. Check that the guest gets the surrogates and no real value
#[test]
fn real_value_split_across_body_chunks_is_masked() {
    run_with_config(injecting(vec![]), |proxy, _, _| async move {
        let accepted = Arc::new(Mutex::new(String::new()));
        let seen = accepted.clone();
        let port = serve(Router::new().route(
            "/",
            get(move |headers: HeaderMap| async move {
                *seen.lock().unwrap() = headers["accept-encoding"].to_str().unwrap().into();
                let text = format!("a {REAL} b {OTHER_REAL} c");
                let cuts = [7, text.len() - 9];
                let chunks = vec![
                    text[..cuts[0]].to_string(),
                    text[cuts[0]..cuts[1]].to_string(),
                    text[cuts[1]..].to_string(),
                ];
                let stream = futures::stream::iter(chunks).then(|chunk| async move {
                    // The pause makes each chunk a separate body frame.
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    Ok::<_, std::convert::Infallible>(chunk)
                });
                Body::from_stream(stream)
            }),
        ))
        .await
        .port();
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&get_with_auth(port, "Accept-Encoding: gzip\r\n"))
            .await;
        assert_eq!(*accepted.lock().unwrap(), "identity");
        let (_, body) = resp.split_once("\r\n\r\n").unwrap();
        assert_eq!(
            dechunk(body),
            format!("a {SURROGATE} b {OTHER_SURROGATE} c")
        );
    });
}

/// Test that the proxy refuses a compressed answer to a request with
/// injected secrets. The proxy cannot find a real value in compressed
/// bytes, so the answer could leak it.
///   1. Start an upstream that sends a gzip body, against the request
///   2. Send a request with the surrogate
///   3. Check the HTTP 502
#[test]
fn compressed_answer_with_injected_secrets_is_refused() {
    run_with_config(injecting(vec![]), |proxy, _, _| async move {
        let port = serve(Router::new().route(
            "/",
            get(|| async { ([("content-encoding", "gzip")], "not really gzip") }),
        ))
        .await
        .port();
        let resp = TestConnection::local(&proxy, port)
            .await
            .roundtrip(&get_with_auth(port, ""))
            .await;
        assert!(resp.starts_with("HTTP/1.1 502"), "{resp}");
        assert!(resp.contains("compressed answer"), "{resp}");
    });
}
