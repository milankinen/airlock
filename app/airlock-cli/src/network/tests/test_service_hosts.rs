//! How the network treats the hosts of a service: the policy rules, the
//! hosts of a service that cannot run, plain HTTP, and host spellings.

use std::rc::Rc;
use std::sync::Arc;

use crate::network::interceptor::Interceptor;
use crate::network::target::{Endpoint, NetworkTarget};
use crate::services::ServiceId;
use crate::services::anthropic::{Anthropic, Endpoints};
use crate::services::auth_codes::PendingCodes;
use crate::services::tokens::TokenKind;
use crate::test_cfg::network::{TestConnection, TestNetworkConfig, build_network, run_with_config};
use crate::test_cfg::provider::{Options, Setup};
use crate::test_cfg::services::{GotLog, answering, insert_grant, production_services, request};
use crate::test_cfg::{block_on_local, test_store, tls_trusting};

/// Test that a service host is allowed and intercepted without an allow
/// rule. A passthrough rule does not stop the interception, but a deny rule
/// wins over the service.
///   1. Check that the service host is allowed and intercepted, and that
///      another port is denied
///   2. Add a passthrough rule and check that the service still intercepts
///   3. Add a deny rule and check that the host is denied
#[test]
fn service_host_is_allowed_and_intercepted_unless_denied() {
    let s = Setup::new(ServiceId::Anthropic, Options::default());
    let port = s.token_port();
    let (_log, _ca, mut network) = build_network(s.config());
    let t = network.resolve_target("127.0.0.1", port);
    assert!(t.allowed && !t.is_passthrough() && t.interceptor.is_some());
    let t = network.resolve_target("127.0.0.1", port.wrapping_add(1));
    assert!(!t.allowed && t.interceptor.is_none());

    network.passthrough_targets = vec![NetworkTarget {
        host: "127.0.0.1".into(),
        port: None,
    }];
    let t = network.resolve_target("127.0.0.1", port);
    assert!(t.allowed && !t.is_passthrough());

    network.deny_targets = vec![NetworkTarget {
        host: "127.0.0.1".into(),
        port: Some(port),
    }];
    let t = network.resolve_target("127.0.0.1", port);
    assert!(!t.allowed && t.interceptor.is_none());
}

/// Test that the hosts of a service that cannot run are denied on all
/// ports, also under an allow-all rule. Otherwise the agent signs in
/// without airlock and gets real tokens.
///   1. Allow all hosts and mark the Anthropic hosts as unavailable
///   2. Check that these hosts are denied on ports 443 and 80, in any
///      spelling
///   3. Check that other hosts are still allowed
#[test]
fn hosts_of_unavailable_service_are_denied_under_allow_all_rule() {
    let cfg = TestNetworkConfig {
        allowed_hosts: vec!["*".into()],
        unavailable_targets: ServiceId::Anthropic.targets(),
        ..Default::default()
    };
    let (_log, _ca, network) = build_network(cfg);
    for host in ["api.anthropic.com", "PLATFORM.claude.com."] {
        let t = network.resolve_target(host, 443);
        assert!(!t.allowed && t.interceptor.is_none(), "{host}");
    }
    assert!(!network.resolve_target("api.anthropic.com", 80).allowed);
    assert!(network.resolve_target("example.com", 443).allowed);
}

/// Test that plain HTTP to a service host is refused. The service handles
/// only TLS, and a real token must never go over an unencrypted
/// connection.
///   1. Store a grant and start a plain TCP upstream on the service host
///   2. Send a plain HTTP request with the surrogate
///   3. Check the 421 answer and that the upstream gets no connection
#[test]
fn plain_http_to_owned_host_is_refused() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_home, store) = test_store();
    let endpoint = Endpoint::new("127.0.0.1", port);
    let service: Rc<dyn Interceptor> = Rc::new(Anthropic::new(
        Endpoints {
            token: endpoint.clone(),
            api: endpoint,
        },
        store.clone(),
        Arc::new(tls_trusting("")),
        PendingCodes::default(),
    ));
    let cfg = TestNetworkConfig {
        interceptors: vec![service],
        ..Default::default()
    };
    run_with_config(cfg, |proxy, _log, _mitm| async move {
        let surrogate = "sk-ant-oat01-airlock-plain";
        insert_grant(
            &store,
            ServiceId::Anthropic,
            &[(TokenKind::Access, "sk-ant-oat01-REAL", surrogate)],
        )
        .await;
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", port)
            .await
            .unwrap();
        let answer = conn
            .roundtrip(&format!(
                "GET /v1/messages HTTP/1.1\r\nHost: 127.0.0.1\r\nAuthorization: Bearer {surrogate}\r\nConnection: close\r\n\r\n"
            ))
            .await;
        assert!(answer.contains("421"), "{answer}");
        // The proxy opens the upstream TCP connection before it reads the
        // request. Check that no request bytes got there.
        if let Ok(Ok((mut sock, _))) =
            tokio::time::timeout(std::time::Duration::from_millis(200), listener.accept()).await
        {
            let mut buf = Vec::new();
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut sock, &mut buf).await;
            assert!(buf.is_empty(), "{}", String::from_utf8_lossy(&buf));
        }
    });
}

/// Test that other spellings of a service host (case, trailing dot) get
/// the same handling as the usual spelling. Otherwise a different spelling
/// can bypass the service.
///   1. Poll for a device code on spellings of the OpenAI token host and
///      check that the guest gets a surrogate code
///   2. Exchange a real code on a spelling of the Anthropic token host and
///      check the local `invalid_grant`
///   3. Send a real token to a spelling of the Anthropic API host and check
///      the local 401
#[test]
fn host_spellings_get_owned_hosts_handling() {
    block_on_local(async {
        let services = production_services();
        for host in ["AUTH.OPENAI.COM", "Auth.OpenAI.com.", "auth.openai.com."] {
            let log = GotLog::default();
            let next = answering(
                &log,
                "application/json",
                r#"{"authorization_code":"real-device-code","code_verifier":"v"}"#,
            );
            let poll = request("POST", "/api/accounts/deviceauth/token", &[], "{}");
            let answer = services
                .send(ServiceId::Openai, host, poll, &[], next)
                .await;
            assert_eq!(answer.status, 200, "{host}: {}", answer.body);
            assert!(!answer.body.contains("real-device-code"), "{host}");
            assert!(answer.body.contains("airlock-code-"), "{host}");
        }

        let log = GotLog::default();
        let exchange = serde_json::json!({
            "grant_type": "authorization_code",
            "code": "real-code",
            "redirect_uri": "http://localhost:40000/callback",
        });
        let req = request(
            "POST",
            "/v1/oauth/token",
            &[("content-type", "application/json")],
            &exchange.to_string(),
        );
        let next = answering(&log, "application/json", "{}");
        let answer = services
            .send(ServiceId::Anthropic, "Platform.Claude.com", req, &[], next)
            .await;
        assert_eq!(answer.status, 400, "{}", answer.body);
        assert!(answer.body.contains("invalid_grant"), "{}", answer.body);

        let req = request(
            "GET",
            "/v1/messages",
            &[("authorization", "Bearer sk-ant-oat01-REAL")],
            "",
        );
        let next = answering(&log, "application/json", "{}");
        let answer = services
            .send(ServiceId::Anthropic, "api.anthropic.com.", req, &[], next)
            .await;
        assert_eq!(answer.status, 401, "{}", answer.body);
        assert!(log.is_empty());
    });
}

/// Test that a guest cannot reach a service host past its interceptor:
/// not on another port, and not with the service host in the `Host`
/// header of a request on another allowed host. A CDN can route such a
/// request to the real service, and the guest then gets real sign-in
/// codes.
///   1. Allow all hosts, and add a service that owns `auth.svc.test:443`
///   2. Check that the service host is denied on port 8443
///   3. On a connection to another allowed host, send requests with the
///      service host in `Host`, in other spellings and with a port
///   4. Check the 421 answers, and that a request with the real host
///      still gets to the upstream
#[test]
fn service_host_on_other_port_or_in_host_header_is_refused() {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (_home, store) = test_store();
    let endpoint = Endpoint::new("auth.svc.test", 443);
    let service: Rc<dyn Interceptor> = Rc::new(Anthropic::new(
        Endpoints {
            token: endpoint.clone(),
            api: endpoint,
        },
        store,
        Arc::new(tls_trusting("")),
        PendingCodes::default(),
    ));
    let cfg = TestNetworkConfig {
        interceptors: vec![service.clone()],
        ..Default::default()
    };
    let (_log, _ca, network) = build_network(cfg);
    assert!(network.resolve_target("auth.svc.test", 443).allowed);
    assert!(!network.resolve_target("AUTH.svc.test.", 8443).allowed);

    let cfg = TestNetworkConfig {
        interceptors: vec![service],
        ..Default::default()
    };
    run_with_config(cfg, |proxy, _log, _mitm| async move {
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let upstream = tokio::spawn(async move {
            let mut requests = Vec::new();
            while let Ok((mut sock, _)) = listener.accept().await {
                // The proxy also connects for the refused requests, but
                // sends no bytes and closes.
                let mut buf = vec![0u8; 4096];
                let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf)
                    .await
                    .unwrap_or(0);
                if n == 0 {
                    continue;
                }
                let request = String::from_utf8_lossy(&buf[..n]).into_owned();
                tokio::io::AsyncWriteExt::write_all(
                    &mut sock,
                    b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\nok",
                )
                .await
                .unwrap();
                requests.push(request);
                break;
            }
            requests
        });

        for host in ["auth.svc.test", "AUTH.svc.test.", "auth.svc.test:8443"] {
            let mut conn = TestConnection::connect(&proxy, "127.0.0.1", port)
                .await
                .unwrap();
            let answer = conn
                .roundtrip(&format!(
                    "POST /api/accounts/deviceauth/token HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n\r\n"
                ))
                .await;
            assert!(answer.contains("421"), "{host}: {answer}");
        }

        let mut conn = TestConnection::connect(&proxy, "127.0.0.1", port)
            .await
            .unwrap();
        let answer = conn
            .roundtrip("GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
            .await;
        assert!(answer.contains("200"), "{answer}");
        let requests = upstream.await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].contains("svc.test"), "{}", requests[0]);
    });
}
