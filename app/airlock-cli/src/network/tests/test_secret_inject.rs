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

fn secret(name: &str, real: &str, surrogate: &str) -> MaskedSecret {
    MaskedSecret {
        name: name.into(),
        real: real.into(),
        surrogate: surrogate.into(),
    }
}

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

fn get_with_auth(port: u16, extra: &str) -> String {
    format!(
        "GET / HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {SURROGATE}\r\n\
         {extra}Connection: close\r\n\r\n"
    )
}

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
