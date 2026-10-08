use std::rc::Rc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use axum::Router;
use axum::routing::{any, get};
use futures::future::LocalBoxFuture;
use http_body_util::Full;
use hyper::{Request, Response};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::network::http::ResponseBody;
use crate::network::interceptor::{Interceptor, Next};
use crate::network::target::{Endpoint, InjectedSecret, NetworkTarget};
use crate::test_cfg::network::*;
use crate::test_cfg::upstream::*;

fn trusting(upstream: &FakeUpstream) -> TestNetworkConfig {
    TestNetworkConfig {
        trust_cas: vec![upstream.ca_pem()],
        ..Default::default()
    }
}

fn get_request(path: &str) -> Request<Full<bytes::Bytes>> {
    Request::get(path).body(Full::default()).unwrap()
}

#[test]
fn https_request_through_mitm_carries_middleware_header_to_upstream() {
    let upstream = FakeUpstream::bind(&[b"http/1.1"]);
    let seen = SeenLog::default();
    let cfg = TestNetworkConfig {
        middleware_scripts: vec![(
            "tls inject",
            r#"req:setHeader("x-tls-injected", "from-lua-over-tls")"#,
        )],
        ..trusting(&upstream)
    };
    run_with_config(cfg, |proxy, _, mitm_ca| async move {
        let port = upstream.port();
        let log = seen.clone();
        upstream.serve(Router::new().route(
            "/hello",
            get(move |req: axum::extract::Request| async move {
                log.record(req).await;
                "tls-ok"
            }),
        ));
        let resp = guest_request(&proxy, &mitm_ca, port, false, get_request("/hello")).await;
        assert_eq!(resp.status, 200);
        assert_eq!(resp.body, "tls-ok");
        assert_eq!(
            seen.last().header("x-tls-injected"),
            Some("from-lua-over-tls")
        );
    });
}

#[test]
fn large_response_through_mitm_arrives_intact_over_h1_and_h2() {
    const SIZE: usize = 8 * 1024 * 1024;
    let expected: String = (0..SIZE)
        .map(|i| char::from(b'a' + (i % 26) as u8))
        .collect();
    for (h2, alpn) in [(false, b"http/1.1".as_slice()), (true, b"h2".as_slice())] {
        let upstream = FakeUpstream::bind(&[alpn]);
        let body = expected.clone();
        let expected = expected.clone();
        run_with_config(trusting(&upstream), |proxy, _, mitm_ca| async move {
            let port = upstream.port();
            upstream.serve(Router::new().route(
                "/big",
                get(move || {
                    let body = body.clone();
                    async move { body }
                }),
            ));
            let resp = guest_request(&proxy, &mitm_ca, port, h2, get_request("/big")).await;
            assert_eq!(resp.status, 200);
            assert_eq!(resp.body.len(), SIZE, "h2={h2}");
            assert!(resp.body == expected, "h2={h2}: body corrupted");
        });
    }
}

#[test]
fn h2_guest_request_to_h1_only_upstream_is_bridged() {
    let upstream = FakeUpstream::bind(&[b"http/1.1"]);
    let seen = SeenLog::default();
    run_with_config(trusting(&upstream), |proxy, _, mitm_ca| async move {
        let port = upstream.port();
        let log = seen.clone();
        upstream.serve(Router::new().route(
            "/echo",
            any(move |req: axum::extract::Request| async move {
                log.record(req).await;
                "bridged"
            }),
        ));
        let resp = guest_request(&proxy, &mitm_ca, port, true, get_request("/echo")).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(resp.body, "bridged");
        let seen = seen.last();
        assert_eq!(seen.method, "GET");
        assert_eq!(seen.path, "/echo");
        assert_eq!(
            seen.header("host"),
            Some(format!("127.0.0.1:{port}").as_str())
        );
    });
}

struct PanicIfCalled {
    targets: Vec<NetworkTarget>,
}

impl Interceptor for PanicIfCalled {
    fn name(&self) -> &'static str {
        "panic-if-called"
    }

    fn targets(&self) -> &[NetworkTarget] {
        &self.targets
    }

    fn send<'a>(
        &'a self,
        _to: &'a Endpoint,
        _req: Request<ResponseBody>,
        _injected: &'a [InjectedSecret],
        _next: Next,
    ) -> LocalBoxFuture<'a, anyhow::Result<Response<ResponseBody>>> {
        Box::pin(async move { panic!("owned host's interceptor ran for non-HTTP bytes") })
    }
}

#[test]
fn non_http_bytes_to_owned_host_are_refused_not_relayed() {
    let upstream = AcceptCounter::bind();
    let port = upstream.port();
    let owned = Rc::new(PanicIfCalled {
        targets: vec![NetworkTarget {
            host: "127.0.0.1".into(),
            port: Some(port),
        }],
    });
    let cfg = TestNetworkConfig {
        interceptors: vec![owned],
        ..Default::default()
    };
    run_with_config(cfg, |proxy, _, mitm_ca| async move {
        let accepted = upstream.start();
        let mut tls = guest_tls(&proxy, &mitm_ca, port, &[]).await;
        tls.write_all(b"NOT AN HTTP REQUEST\r\n").await.unwrap();
        let mut resp = Vec::new();
        let _ = tokio::time::timeout(Duration::from_secs(2), tls.read_to_end(&mut resp))
            .await
            .expect("refused connection closes");
        assert!(resp.is_empty(), "{resp:?}");
        assert_eq!(accepted.load(Ordering::SeqCst), 0);
    });
}
