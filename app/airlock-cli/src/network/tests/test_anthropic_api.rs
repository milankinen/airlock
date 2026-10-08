//! Anthropic API calls through the proxy: the swap of surrogates for real
//! credentials, the refusal of unknown credentials, and the scan of answers.

use bytes::Bytes;
use http_body_util::Full;
use hyper::Request;

use crate::services::ServiceId;
use crate::test_cfg::block_on_local;
use crate::test_cfg::network::{build_network, start_rpc};
use crate::test_cfg::provider::*;
use crate::test_cfg::services::masked;
use crate::test_cfg::upstream::{get_with_bearer, post};

/// A GET request of `path` with `headers` and an empty body.
fn get_with(path: &str, headers: &[(&str, &str)]) -> Request<Full<Bytes>> {
    let mut req = Request::get(path);
    for (name, value) in headers {
        req = req.header(*name, *value);
    }
    req.body(Full::new(Bytes::new())).unwrap()
}

/// Test that an API request gets the real token and goes only to the API
/// endpoint. A forged `Host` or absolute URI must not send the real token to
/// a different host.
///   1. Sign in
///   2. Send API requests over HTTP/1.1 and HTTP/2, some with an attacker
///      host in the `Host` header or in the URI
///   3. Check that the provider gets the real token, its own authority, the
///      path and at most one `Host` header
#[test]
fn api_request_over_h1_and_h2_carries_real_token_to_endpoint_authority() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let bearer = format!("Bearer {}", answer["access_token"].as_str().unwrap());
        let endpoint = format!("127.0.0.1:{}", r.api_port);
        for (h2, uri, host) in [
            (false, "/v1/messages", None),
            (true, "/v1/messages", None),
            (false, "/v1/messages", Some("attacker.example")),
            (
                false,
                "https://attacker.example/v1/messages",
                Some("attacker.example"),
            ),
            (true, "https://attacker.example/v1/messages", None),
            (
                true,
                "https://attacker.example:1/v1/messages",
                Some("other.example"),
            ),
        ] {
            let mut req = Request::get(uri).header("authorization", &bearer);
            if let Some(host) = host {
                req = req.header("host", host);
            }
            let req = req.body(Full::new(Bytes::new())).unwrap();
            let resp = r.request(r.api_port, h2, req).await;
            assert_eq!(resp.status, 200, "h2={h2} {uri}: {}", resp.body);
            let seen = r.last_seen();
            assert_eq!(
                seen.header("authorization"),
                Some("Bearer sk-ant-oat01-REAL-ACCESS-0"),
                "h2={h2} {uri}"
            );
            assert_eq!(seen.authority.as_deref(), Some(endpoint.as_str()), "{uri}");
            assert_eq!(seen.path, "/v1/messages");
            assert!(seen.headers.get_all("host").iter().count() <= 1, "{uri}");
        }
    });
}

/// Test that the proxy asks for uncompressed API answers and refuses answers
/// that it cannot scan or that carry a real token.
///   1. Sign in and send an API request that accepts gzip and brotli
///   2. Check that the provider gets `Accept-Encoding: identity`
///   3. Get gzip answers and an answer with the real refresh token
///   4. Check that each one becomes a local 502 without a real token
#[test]
fn api_answers_come_uncompressed_without_real_tokens_or_not_at_all() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let bearer = format!("Bearer {}", answer["access_token"].as_str().unwrap());
        let get = |path| {
            get_with(
                path,
                &[("authorization", &bearer), ("accept-encoding", "gzip, br")],
            )
        };
        let resp = r.on_api(get("/v1/messages")).await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(r.last_seen().header("accept-encoding"), Some("identity"));
        for path in ["/v1/gzip/json", "/v1/gzip/sse", "/v1/leak/refresh"] {
            let resp = r.on_api(get(path)).await;
            assert_eq!(resp.status, 502, "{path}: {}", resp.body);
            assert!(!resp.body.contains("REAL"), "{path}: {}", resp.body);
        }
    });
}

/// Test that the proxy swaps surrogates only on the API host. The token host
/// must not forward API paths, and the API host must not swap a refresh
/// surrogate in a body.
///   1. Sign in with the API on its own host and check an API call
///   2. Send the API call to the token host and check the local 403
///   3. Send a refresh to the API host
///   4. Check that the provider gets the refresh surrogate, not the real
///      token
#[test]
fn surrogates_are_swapped_on_api_host_only() {
    let opts = Options {
        api: ApiHost::Own,
        ..Options::default()
    };
    Setup::new(ServiceId::Anthropic, opts).run(|r| async move {
        let answer = r.sign_in().await;
        let access = answer["access_token"].as_str().unwrap();
        assert_eq!(r.api_get("/v1/messages", access).await.status, 200);

        let before = r.upstream_requests();
        let resp = r
            .on_token_host(get_with_bearer("/v1/messages", access))
            .await;
        assert_eq!(resp.status, 403, "{}", resp.body);
        assert_eq!(resp.json()["error"], "airlock_route_not_allowed");
        assert_eq!(r.upstream_requests(), before);

        let refresh = serde_json::json!({
            "grant_type": "refresh_token",
            "refresh_token": answer["refresh_token"],
        });
        let resp = r
            .on_api(post(
                "/v1/oauth/token",
                "application/json",
                &refresh.to_string(),
            ))
            .await;
        // The fake provider refuses the surrogate with invalid_grant.
        assert_eq!(resp.status, 400);
        assert!(r.last_seen().body.contains("sk-ant-ort01-airlock-"));
    });
}

/// Test that the API host refuses unknown credentials locally, with a
/// message that tells the user what to do.
///   1. Sign in
///   2. Send bearer tokens that are an unknown surrogate, the real token and
///      an unknown token, and check each 401 message
///   3. Send a real API key, an unknown API key surrogate and Basic auth, and
///      check each 401
///   4. Check that no request goes upstream
#[test]
fn unknown_credentials_on_api_host_are_refused_locally() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        r.sign_in().await;
        let before = r.upstream_requests();
        for (unknown, says) in [
            ("sk-ant-oat01-airlock-unknown", "sign in again"),
            (
                "sk-ant-oat01-REAL-ACCESS-0",
                "[network.services] anthropic enabled",
            ),
            ("sk-ant-oat01-unknown", "masked [env] secret with inject"),
        ] {
            let resp = r.api_get("/v1/messages", unknown).await;
            assert_eq!(resp.status, 401, "{unknown}");
            assert!(resp.body.contains(says), "{unknown}: {}", resp.body);
        }
        for (name, value) in [
            ("x-api-key", REAL_API_KEY),
            ("x-api-key", "sk-ant-api03-airlock-unknown"),
            ("authorization", "Basic dXNlcjpwYXNz"),
        ] {
            let resp = r.on_api(get_with("/v1/messages", &[(name, value)])).await;
            assert_eq!(resp.status, 401, "{value}: {}", resp.body);
        }
        assert_eq!(r.upstream_requests(), before);
    });
}

/// Test that API keys from `create_api_key` reach the guest as surrogates,
/// and that a grant can create only a limited number of keys.
///   1. Sign in and create three API keys
///   2. Check that each answer has a surrogate key and that the key works
///      for an API call with the real key
///   3. Create a fourth key and check the local 429 that does not go
///      upstream
#[test]
fn created_api_keys_reach_guest_as_surrogates_up_to_limit() {
    Setup::new(ServiceId::Anthropic, Options::default()).run(|r| async move {
        let answer = r.sign_in().await;
        let access = answer["access_token"].as_str().unwrap();
        for _ in 0..3 {
            let resp = r.create_api_key(access).await;
            assert_eq!(resp.status, 200, "{}", resp.body);
            assert!(!resp.body.contains("REAL"), "{}", resp.body);
            assert_eq!(resp.json()["name"], "claude-code");
            let key = resp.json()["raw_key"].as_str().unwrap().to_string();
            assert!(key.starts_with("sk-ant-api03-airlock-"), "{key}");

            let resp = r
                .on_api(get_with("/v1/messages", &[("x-api-key", &key)]))
                .await;
            assert_eq!(resp.status, 200, "{}", resp.body);
            assert_eq!(r.last_seen().header("x-api-key"), Some(REAL_API_KEY));
        }
        let before = r.upstream_requests();
        let resp = r.create_api_key(access).await;
        assert_eq!(resp.status, 429, "{}", resp.body);
        assert_eq!(resp.json()["error"]["type"], "rate_limit_error");
        assert_eq!(r.upstream_requests(), before);
    });
}

/// Test that masked secrets with inject rules work on the API host without a
/// sign-in. The user can give their own key or token instead of a sign-in.
///   1. Configure masked secrets for an API key and an OAuth token
///   2. Send API calls with the masked values
///   3. Check that they go upstream with the real values and that no grant
///      is made
#[test]
fn injected_secrets_work_without_sign_in() {
    let s = Setup::new(ServiceId::Anthropic, Options::default());
    let mut cfg = s.config();
    cfg.allowed_hosts = vec!["*".into()];
    cfg.inject = vec![
        masked("ANTHROPIC_API_KEY", REAL_API_KEY, "masked-api-key"),
        masked(
            "CLAUDE_CODE_OAUTH_TOKEN",
            "sk-ant-oat01-REAL-ACCESS-0",
            "masked-oauth-token",
        ),
    ];
    s.run_with(cfg, |r| async move {
        let resp = r
            .on_api(get_with("/v1/messages", &[("x-api-key", "masked-api-key")]))
            .await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert_eq!(r.last_seen().header("x-api-key"), Some(REAL_API_KEY));
        let resp = r.api_get("/v1/messages", "masked-oauth-token").await;
        assert_eq!(resp.status, 200, "{}", resp.body);
        assert!(r.grants().await.is_empty());
    });
}

/// Test that middleware scripts and the monitor see only the surrogate. The
/// swap happens after them, so that no real token gets into logs or events.
///   1. Add a middleware script that logs the `authorization` header
///   2. Sign in and send an API call
///   3. Check that the provider gets the real token
///   4. Check that the script log and the monitor events have the surrogate
#[test]
fn middleware_and_monitor_see_only_surrogate() {
    let s = Setup::new(ServiceId::Anthropic, Options::default());
    let mut cfg = s.config();
    cfg.allowed_hosts = vec!["127.0.0.1".into()];
    cfg.middleware_scripts = vec![(
        "log auth",
        r#"if req:header("authorization") then log(req:header("authorization")) end"#,
    )];
    block_on_local(async move {
        let (log, mitm, network) = build_network(cfg);
        let mut events = network.handle().events();
        let r = s.serve(start_rpc(network), mitm);
        let answer = r.sign_in().await;
        let access = answer["access_token"].as_str().unwrap();
        assert_eq!(r.api_get("/v1/messages", access).await.status, 200);
        assert_eq!(
            r.last_seen().header("authorization"),
            Some("Bearer sk-ant-oat01-REAL-ACCESS-0")
        );
        assert_eq!(log.messages(), [format!("Bearer {access}")]);
        let mut auth_headers = Vec::new();
        while let Ok(event) = events.try_recv() {
            if let airlock_monitor::NetworkEvent::Request(req) = event {
                auth_headers.extend(
                    req.headers
                        .iter()
                        .filter(|(k, _)| k == "authorization")
                        .map(|(_, v)| v.clone()),
                );
            }
        }
        assert_eq!(auth_headers, [format!("Bearer {access}")]);
    });
}
