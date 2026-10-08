//! The credential swap on the API hosts: which headers get the real token,
//! and which credentials the services refuse locally.

use crate::network::target::InjectedSecret;
use crate::services::ServiceId;
use crate::services::tokens::{FAKE_JWT_PREFIX, TokenKind};
use crate::test_cfg::block_on_local;
use crate::test_cfg::services::{
    GotLog, ProductionServices, answering, insert_grant, masked, production_services, request,
};

/// Access token surrogate of [`store_anthropic_grant`].
const ACCESS: &str = "sk-ant-oat01-airlock-KNOWNACCESS";
/// API key surrogate of [`store_anthropic_grant`].
const KEY: &str = "sk-ant-api03-airlock-KNOWNKEY";

/// Store an Anthropic grant with the access token surrogate [`ACCESS`]
/// and the API key surrogate [`KEY`].
async fn store_anthropic_grant(services: &ProductionServices) {
    insert_grant(
        &services.store,
        ServiceId::Anthropic,
        &[
            (TokenKind::Access, "sk-ant-oat01-REAL", ACCESS),
            (TokenKind::ApiKey, "sk-ant-api03-REALKEY", KEY),
        ],
    )
    .await;
}

/// Test that the API host swaps surrogates only in the credential headers,
/// on every path. A surrogate in other headers or in the body must stay a
/// surrogate, so that the guest cannot make the proxy echo a real token.
///   1. Store an Anthropic grant with an access token and an API key
///   2. Send requests with surrogates in credential headers, other headers
///      and the body, on normal and odd paths
///   3. Check that only `authorization` and `x-api-key` get the real values
///   4. Check that a malformed API key gets a local 401
#[test]
fn credential_surrogates_are_swapped_on_every_api_path_only() {
    block_on_local(async {
        let services = production_services();
        store_anthropic_grant(&services).await;
        for path in ["/v1/messages", "/api/oauth/claude_cli/roles", "/x/../y?z=1"] {
            let log = GotLog::default();
            let body = format!(r#"{{"token":"{ACCESS}"}}"#);
            let origin = format!("https://{ACCESS}");
            let bearer = format!("Bearer {ACCESS}");
            let req = request(
                "POST",
                path,
                &[
                    ("authorization", &bearer),
                    ("x-api-key", KEY),
                    ("origin", &origin),
                    ("anthropic-beta", ACCESS),
                ],
                &body,
            );
            let next = answering(&log, "text/plain", "ok");
            let answer = services
                .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
                .await;
            assert_eq!(answer.status, 200, "{path}");
            let got = log.all()[0].clone();
            assert_eq!(
                got.header("authorization"),
                Some("Bearer sk-ant-oat01-REAL")
            );
            assert!(got.headers.get("authorization").unwrap().is_sensitive());
            assert_eq!(got.header("x-api-key"), Some("sk-ant-api03-REALKEY"));
            assert_eq!(got.header("origin"), Some(origin.as_str()));
            assert_eq!(got.header("anthropic-beta"), Some(ACCESS));
            assert_eq!(got.body, body, "{path}");
        }

        // A surrogate with more text after it is not a known credential.
        let log = GotLog::default();
        let with_extra = format!("{KEY} extra");
        let req = request("GET", "/v1/messages", &[("x-api-key", &with_extra)], "");
        let next = answering(&log, "text/plain", "ok");
        let answer = services
            .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
            .await;
        assert_eq!(answer.status, 401);
        assert!(log.is_empty());
    });
}

/// Test that the token host and the hosts of other services do not swap
/// the Anthropic surrogates. Only the API host of the grant swaps them.
///   1. Store an Anthropic grant with an access token
///   2. Send a request with the surrogate to the Anthropic token host and
///      check that it goes upstream unchanged
///   3. Send requests with the surrogate to the ChatGPT host, as a
///      credential and in another header
///   4. Check the local 401 for the credential and the unchanged header
#[test]
fn token_and_other_service_hosts_do_not_swap_anthropic_surrogates() {
    block_on_local(async {
        let services = production_services();
        store_anthropic_grant(&services).await;

        // platform.claude.com is the token host. It forwards the request but
        // does not swap.
        let log = GotLog::default();
        let req = request("GET", "/v1/oauth/hello", &[("x-custom", ACCESS)], "");
        let next = answering(&log, "text/plain", "ok");
        services
            .send(ServiceId::Anthropic, "platform.claude.com", req, &[], next)
            .await;
        assert_eq!(log.all()[0].header("x-custom"), Some(ACCESS));

        // The OpenAI service does not know the Anthropic surrogate.
        let log = GotLog::default();
        let bearer = format!("Bearer {ACCESS}");
        let req = request("GET", "/backend-api/x", &[("authorization", &bearer)], "");
        let next = answering(&log, "text/plain", "ok");
        let answer = services
            .send(ServiceId::Openai, "chatgpt.com", req, &[], next)
            .await;
        assert_eq!(answer.status, 401);
        assert!(log.is_empty());
        let req = request("GET", "/backend-api/x", &[("x-custom", ACCESS)], "");
        let next = answering(&log, "text/plain", "ok");
        services
            .send(ServiceId::Openai, "chatgpt.com", req, &[], next)
            .await;
        assert_eq!(log.all()[0].header("x-custom"), Some(ACCESS));
    });
}

/// Test that refresh and ID surrogates do not work as API credentials. Only
/// access tokens and API keys are credentials of an API request.
///   1. Store an Anthropic grant with a refresh token and an OpenAI grant
///      with an ID token
///   2. Send API requests with these surrogates as credentials
///   3. Check that each request gets a local 401 with
///      `airlock_foreign_credential` and does not go upstream
#[test]
fn refresh_and_id_surrogates_are_no_api_credentials() {
    block_on_local(async {
        let services = production_services();
        insert_grant(
            &services.store,
            ServiceId::Anthropic,
            &[
                (
                    TokenKind::Access,
                    "sk-ant-oat01-REAL",
                    "sk-ant-oat01-airlock-A",
                ),
                (
                    TokenKind::Refresh,
                    "sk-ant-ort01-REAL",
                    "sk-ant-ort01-airlock-R",
                ),
            ],
        )
        .await;
        let fake_id = format!("{FAKE_JWT_PREFIX}e30.sig");
        insert_grant(
            &services.store,
            ServiceId::Openai,
            &[
                (TokenKind::Access, "REAL-ACCESS-OPAQUE-1", "airlock-at-A"),
                (TokenKind::Id, "REAL-ID", &fake_id),
            ],
        )
        .await;
        for (service, host, name, value) in [
            (
                ServiceId::Anthropic,
                "api.anthropic.com",
                "authorization",
                "Bearer sk-ant-ort01-airlock-R".to_string(),
            ),
            (
                ServiceId::Anthropic,
                "api.anthropic.com",
                "x-api-key",
                "sk-ant-ort01-airlock-R".to_string(),
            ),
            (
                ServiceId::Openai,
                "chatgpt.com",
                "authorization",
                format!("Bearer {fake_id}"),
            ),
        ] {
            let log = GotLog::default();
            let req = request("GET", "/v1/messages", &[(name, &value)], "");
            let next = answering(&log, "text/plain", "ok");
            let answer = services.send(service, host, req, &[], next).await;
            assert_eq!(answer.status, 401, "{value}: {}", answer.body);
            assert!(
                answer.body.contains("airlock_foreign_credential"),
                "{}",
                answer.body
            );
            assert!(log.is_empty(), "{value}");
        }
    });
}

/// Test that the ChatGPT host accepts only surrogates or injected secrets as
/// credentials. A real token that the guest knows by other means must not
/// pass.
///   1. Send a real token without an inject rule and check the local 401
///   2. Send the same token with a matching injected secret and check that
///      it goes upstream
///   3. Send an unknown surrogate with an injected secret and check the
///      local 401
#[test]
fn chatgpt_credentials_must_be_surrogates_or_injected_secrets() {
    block_on_local(async {
        let services = production_services();
        let secret = InjectedSecret::new(masked("CHATGPT_TOKEN", "real-chatgpt-token", "masked"));
        // The inject rules replace the masked value before the service sees
        // the request. Thus the service gets the real value of the secret.
        for (token, injected, status) in [
            ("real-chatgpt-token", vec![], 401),
            ("real-chatgpt-token", vec![secret.clone()], 200),
            ("airlock-at-unknown", vec![secret], 401),
        ] {
            let log = GotLog::default();
            let bearer = format!("Bearer {token}");
            let req = request(
                "GET",
                "/backend-api/codex/models",
                &[("authorization", &bearer)],
                "",
            );
            let next = answering(&log, "text/plain", "ok");
            let answer = services
                .send(ServiceId::Openai, "chatgpt.com", req, &injected, next)
                .await;
            assert_eq!(answer.status, status, "{token}: {}", answer.body);
            assert_eq!(log.len(), usize::from(status == 200), "{token}");
        }
    });
}
