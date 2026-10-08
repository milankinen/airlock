use crate::network::target::InjectedSecret;
use crate::services::ServiceId;
use crate::services::tokens::{FAKE_JWT_PREFIX, TokenKind};
use crate::test_cfg::block_on_local;
use crate::test_cfg::services::{
    GotLog, answering, insert_grant, masked, production_services, request,
};

#[test]
fn credential_surrogates_are_swapped_on_every_api_path_only() {
    block_on_local(async {
        let services = production_services();
        let access = "sk-ant-oat01-airlock-KNOWNACCESS";
        let key = "sk-ant-api03-airlock-KNOWNKEY";
        insert_grant(
            &services.store,
            ServiceId::Anthropic,
            &[
                (TokenKind::Access, "sk-ant-oat01-REAL", access),
                (TokenKind::ApiKey, "sk-ant-api03-REALKEY", key),
            ],
        )
        .await;
        for path in ["/v1/messages", "/api/oauth/claude_cli/roles", "/x/../y?z=1"] {
            let log = GotLog::default();
            let body = format!(r#"{{"token":"{access}"}}"#);
            let origin = format!("https://{access}");
            let bearer = format!("Bearer {access}");
            let req = request(
                "POST",
                path,
                &[
                    ("authorization", &bearer),
                    ("x-api-key", key),
                    ("origin", &origin),
                    ("anthropic-beta", access),
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
            assert_eq!(got.header("anthropic-beta"), Some(access));
            assert_eq!(got.body, body, "{path}");
        }

        let log = GotLog::default();
        let with_extra = format!("{key} extra");
        let req = request("GET", "/v1/messages", &[("x-api-key", &with_extra)], "");
        let next = answering(&log, "text/plain", "ok");
        let answer = services
            .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
            .await;
        assert_eq!(answer.status, 401);
        assert!(log.is_empty());

        let req = request("GET", "/v1/oauth/hello", &[("x-custom", access)], "");
        let next = answering(&log, "text/plain", "ok");
        services
            .send(ServiceId::Anthropic, "platform.claude.com", req, &[], next)
            .await;
        assert_eq!(log.all()[0].header("x-custom"), Some(access));

        let log = GotLog::default();
        let bearer = format!("Bearer {access}");
        let req = request("GET", "/backend-api/x", &[("authorization", &bearer)], "");
        let next = answering(&log, "text/plain", "ok");
        let answer = services
            .send(ServiceId::Openai, "chatgpt.com", req, &[], next)
            .await;
        assert_eq!(answer.status, 401);
        assert!(log.is_empty());
        let req = request("GET", "/backend-api/x", &[("x-custom", access)], "");
        let next = answering(&log, "text/plain", "ok");
        services
            .send(ServiceId::Openai, "chatgpt.com", req, &[], next)
            .await;
        assert_eq!(log.all()[0].header("x-custom"), Some(access));
    });
}

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

#[test]
fn chatgpt_credentials_must_be_surrogates_or_injected_secrets() {
    block_on_local(async {
        let services = production_services();
        let secret = InjectedSecret::new(masked("CHATGPT_TOKEN", "real-chatgpt-token", "masked"));
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
