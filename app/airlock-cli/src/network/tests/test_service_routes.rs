//! The routes of the service hosts: which paths the token hosts forward,
//! how the proxy rewrites them, and the last check of their answers.

use hyper::Version;
use serde_json::json;

use crate::services::ServiceId;
use crate::test_cfg::block_on_local;
use crate::test_cfg::services::{GotLog, answering, production_services, request};

/// Test that a service refuses a token answer from a host that it does not
/// know. The request still goes upstream, but the token must not get to
/// the sandbox.
///   1. Send a request of each service to an unknown host
///   2. Let the upstream answer with an access token
///   3. Check the local 502 without the token
#[test]
fn unknown_endpoint_answer_with_token_is_refused() {
    block_on_local(async {
        let services = production_services();
        for service in ServiceId::ALL {
            let next = answering(
                &GotLog::default(),
                "application/json",
                r#"{"access_token":"real"}"#,
            );
            let answer = services
                .send(
                    service,
                    "other.example",
                    request("GET", "/x", &[], ""),
                    &[],
                    next,
                )
                .await;
            assert_eq!(answer.status, 502, "{}", answer.body);
            assert!(!answer.body.contains("real"), "{}", answer.body);
        }
    });
}

/// Test that answers of the forwarded token host routes pass the last check
/// unless they carry a token or a code.
///   1. Send requests to the forwarded routes of both token hosts
///   2. Let the upstream answer with token fields, codes or a verifier, and
///      check the local 502
///   3. Let the upstream answer with surrogate codes, other fields, HTML or
///      large JSON, and check that they pass
#[test]
fn token_host_routes_pass_backstop_unless_answer_carries_token() {
    block_on_local(async {
        let services = production_services();
        // The check reads only JSON answers up to a size limit. A larger
        // answer with a known length passes unchanged.
        let big = json!({ "access_token": "x", "pad": "a".repeat(70 * 1024) }).to_string();
        for (content_type, answer, refused) in [
            (
                "application/json",
                json!({ "accessToken": "x", "access_token": "real" }).to_string(),
                true,
            ),
            (
                "application/json",
                json!({ "code": "real-code" }).to_string(),
                true,
            ),
            (
                "application/json",
                json!({ "authorization_code": "real-code" }).to_string(),
                true,
            ),
            (
                "application/json",
                json!({ "data": { "code_verifier": "v" } }).to_string(),
                true,
            ),
            (
                "application/json",
                json!({ "code": "airlock-code-x" }).to_string(),
                false,
            ),
            (
                "application/json",
                json!({ "user_code": "U", "device_auth_id": "d" }).to_string(),
                false,
            ),
            (
                "text/html",
                "<p>access_token sk-ant-oat01-x</p>".to_string(),
                false,
            ),
            ("application/json", big, false),
        ] {
            for (service, host, method, path) in [
                (
                    ServiceId::Openai,
                    "auth.openai.com",
                    "POST",
                    "/api/accounts/deviceauth/usercode",
                ),
                (
                    ServiceId::Anthropic,
                    "platform.claude.com",
                    "GET",
                    "/v1/oauth/hello",
                ),
            ] {
                let log = GotLog::default();
                let next = answering(&log, content_type, &answer);
                let got = services
                    .send(service, host, request(method, path, &[], "{}"), &[], next)
                    .await;
                assert_eq!(got.status == 502, refused, "{path}: {answer:.80}");
                assert_eq!(log.len(), 1, "{path}");
            }
        }
    });
}

/// Test that an allowed route goes upstream with its own normal path only.
/// The guest must not send a different path or a query through an allowed
/// route.
///   1. Send requests with odd case, dot segments, encoded characters,
///      queries and parameters, over HTTP/1.1 and HTTP/2
///   2. Check that the upstream gets only the normal path of the route
#[test]
fn allowed_routes_forward_their_own_path_only() {
    block_on_local(async {
        let services = production_services();
        for (service, host, method, raw, h2, want) in [
            (
                ServiceId::Anthropic,
                "platform.claude.com",
                "GET",
                "/V1//oauth/x/../hello?next=/v1/messages",
                false,
                "/v1/oauth/hello",
            ),
            (
                ServiceId::Anthropic,
                "platform.claude.com",
                "GET",
                "https://platform.claude.com/x%2f..%2f/V1//oauth/hello?a",
                true,
                "https://platform.claude.com/v1/oauth/hello",
            ),
            (
                ServiceId::Openai,
                "auth.openai.com",
                "POST",
                "/api/accounts/deviceauth/usercode/?a=1",
                false,
                "/api/accounts/deviceauth/usercode",
            ),
            (
                ServiceId::Openai,
                "auth.openai.com",
                "POST",
                "/api/accounts/%64eviceauth/token;x",
                false,
                "/api/accounts/deviceauth/token",
            ),
        ] {
            let log = GotLog::default();
            let mut req = request(method, raw, &[], "{}");
            if h2 {
                *req.version_mut() = Version::HTTP_2;
            }
            let next = answering(&log, "application/json", r#"{"authorization_code":"c"}"#);
            services.send(service, host, req, &[], next).await;
            assert_eq!(log.all()[0].uri, want, "{raw}");
        }
    });
}

/// Test that the token hosts refuse every other route, and that the API
/// hosts take every path.
///   1. Send requests with wrong methods or other paths to both token hosts
///   2. Check the local 403 `airlock_route_not_allowed` and that nothing
///      goes upstream
///   3. Send a request of any path to both API hosts and check the 200
#[test]
fn other_token_host_routes_are_forbidden_and_api_hosts_take_every_path() {
    block_on_local(async {
        let services = production_services();
        for (service, host, method, path) in [
            (ServiceId::Openai, "auth.openai.com", "GET", "/"),
            (ServiceId::Openai, "auth.openai.com", "GET", "/oauth/token"),
            (
                ServiceId::Openai,
                "auth.openai.com",
                "POST",
                "/api/accounts/deviceauth/other",
            ),
            (
                ServiceId::Openai,
                "auth.openai.com",
                "GET",
                "/api/accounts/deviceauth/usercode",
            ),
            (
                ServiceId::Openai,
                "auth.openai.com",
                "POST",
                "/backend-api/codex/models",
            ),
            (
                ServiceId::Anthropic,
                "Platform.Claude.com.",
                "GET",
                "/v1/oauth/token",
            ),
            (
                ServiceId::Anthropic,
                "Platform.Claude.com.",
                "POST",
                "/v1/oauth/hello",
            ),
            (
                ServiceId::Anthropic,
                "Platform.Claude.com.",
                "GET",
                "/v1/messages",
            ),
            (
                ServiceId::Anthropic,
                "Platform.Claude.com.",
                "GET",
                "/oauth/code/callback",
            ),
        ] {
            let log = GotLog::default();
            let next = answering(&log, "text/plain", "ok");
            let answer = services
                .send(service, host, request(method, path, &[], ""), &[], next)
                .await;
            assert_eq!(answer.status, 403, "{method} {path}: {}", answer.body);
            assert_eq!(answer.json()["error"], "airlock_route_not_allowed");
            assert!(log.is_empty(), "{method} {path}");
        }
        for (service, host) in [
            (ServiceId::Openai, "chatgpt.com"),
            (ServiceId::Anthropic, "api.anthropic.com"),
        ] {
            let next = answering(&GotLog::default(), "text/plain", "ok");
            let answer = services
                .send(
                    service,
                    host,
                    request("GET", "/anything", &[], ""),
                    &[],
                    next,
                )
                .await;
            assert_eq!(answer.status, 200, "{host}");
        }
    });
}
