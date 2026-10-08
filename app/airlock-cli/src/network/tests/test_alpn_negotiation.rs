//! Tests for ALPN negotiation in the TLS MITM: the guest and the upstream
//! must agree on the same protocol through the proxy.

use axum::Router;
use axum::routing::get;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::test_cfg::network::*;
use crate::test_cfg::upstream::*;

const H1: &[u8] = b"http/1.1";
const H2: &[u8] = b"h2";

type Alpn = &'static [&'static [u8]];

/// Test that the MITM negotiates ALPN with the guest from what the guest
/// and the upstream both offer. A wrong protocol makes the guest and the
/// upstream speak different HTTP versions.
///   1. Start one TLS upstream for each ALPN case
///   2. Connect the guest over TLS with its ALPN offer
///   3. Check the negotiated protocol (none when the upstream offers none)
///   4. For HTTP/1.1 cases, send a GET and check the response
#[test]
fn mitm_negotiates_alpn_from_guest_offer() {
    // Each case: upstream ALPN, guest ALPN, expected negotiated protocol.
    let cases: [(Alpn, Alpn, Option<&[u8]>); 4] = [
        (&[H1], &[H1], Some(H1)),
        (&[H2, H1], &[H1], Some(H1)),
        (&[H2, H1], &[], None),
        (&[H2], &[H2], Some(H2)),
    ];
    let upstreams: Vec<FakeUpstream> = cases
        .iter()
        .map(|(upstream_alpn, _, _)| FakeUpstream::bind(upstream_alpn))
        .collect();
    let cfg = TestNetworkConfig {
        trust_cas: upstreams.iter().map(FakeUpstream::ca_pem).collect(),
        ..Default::default()
    };
    run_with_config(cfg, |proxy, _, mitm_ca| async move {
        for (upstream, (upstream_alpn, guest_alpn, expected)) in upstreams.into_iter().zip(cases) {
            let port = upstream.port();
            upstream.serve(Router::new().route("/", get(|| async { "alpn-ok" })));
            let mut tls = guest_tls(&proxy, &mitm_ca, port, guest_alpn).await;
            let negotiated = tls.get_ref().1.alpn_protocol().map(<[u8]>::to_vec);
            assert_eq!(
                negotiated.as_deref(),
                expected,
                "upstream {upstream_alpn:?}, guest {guest_alpn:?}"
            );
            // The guest sends raw HTTP/1.1 below, so skip the h2 case.
            if expected == Some(H2) {
                continue;
            }
            tls.write_all(http_get(port, "/").as_bytes()).await.unwrap();
            let mut resp = Vec::new();
            let _ = tls.read_to_end(&mut resp).await;
            let resp = String::from_utf8_lossy(&resp);
            assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
            assert!(resp.ends_with("alpn-ok"), "{resp}");
        }
    });
}
