use bytes::Bytes;
use hyper::body::Frame;
use hyper::{HeaderMap, Response, StatusCode};

use crate::network::http::ResponseBody;
use crate::network::interceptor::Next;
use crate::network::target::InjectedSecret;
use crate::services::ServiceId;
use crate::services::tokens::TokenKind;
use crate::test_cfg::block_on_local;
use crate::test_cfg::services::{
    Answer, GotLog, ProductionServices, answering, insert_grant, masked, production_services,
    request, shaped_token, streaming, streaming_frames,
};

async fn get_messages(
    services: &ProductionServices,
    injected: &[InjectedSecret],
    next: Next,
) -> Answer {
    let req = request("GET", "/v1/messages", &[], "");
    services
        .send(
            ServiceId::Anthropic,
            "api.anthropic.com",
            req,
            injected,
            next,
        )
        .await
}

#[test]
fn api_answers_are_scanned_as_they_stream() {
    block_on_local(async {
        let services = production_services();
        insert_grant(
            &services.store,
            ServiceId::Anthropic,
            &[(
                TokenKind::Refresh,
                "sk-ant-ort01-REAL-REFRESH",
                "sk-ant-ort01-airlock-R",
            )],
        )
        .await;
        let token = shaped_token("sk-ant-api03");
        let (x, y) = token.split_at(20);
        let answer = get_messages(
            &services,
            &[],
            streaming(
                &[&format!(r#"{{"key":"{x}"#), &format!(r#"{y}"}}"#)],
                "application/json",
            ),
        )
        .await;
        assert_eq!(answer.status, 502);

        let answer = get_messages(
            &services,
            &[],
            streaming(
                &[
                    "event: a\ndata: {}\n\n",
                    "event: b\ndata: {\"t\":\"sk-ant-ort01-REAL-",
                    "REFRESH\"}\n\n",
                    "event: c\ndata: {}\n\n",
                ],
                "text/event-stream",
            ),
        )
        .await;
        assert_eq!((answer.status, answer.failed), (StatusCode::OK, true));
        assert_eq!(
            answer.body,
            "event: a\ndata: {}\n\nevent: b\ndata: {\"t\":\""
        );

        let model = format!("data: {{\"text\":\"an example key: {token}\"}}\n\n");
        let answer = get_messages(&services, &[], streaming(&[&model], "text/event-stream")).await;
        assert!(!answer.refused());
        assert_eq!(answer.body, model);

        let line = format!(
            "data: {{\"k\":\"sk-ant-oat01-airlock-{}\"}}\n\n",
            "x".repeat(50)
        );
        let chunks: Vec<String> = (0..300).map(|_| line.clone()).collect();
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let answer = get_messages(&services, &[], streaming(&refs, "text/event-stream")).await;
        assert!(!answer.refused());
        assert_eq!(answer.body, chunks.concat());

        let pad = "a ".repeat(50 * 1024);
        let answer = get_messages(
            &services,
            &[],
            streaming(
                &[r#"{"pad":""#, &pad, &format!(r#"","k":"{token}"}}"#)],
                "application/json",
            ),
        )
        .await;
        assert_eq!((answer.status, answer.failed), (StatusCode::OK, true));
        assert!(!answer.body.contains("RRRR"));
        assert!(answer.body.len() >= 100 * 1024);

        let big = format!(r#"{{"pad":"{}"}}"#, "a".repeat(100 * 1024));
        let next = answering(&GotLog::default(), "application/json", &big);
        let answer = get_messages(&services, &[], next).await;
        assert!(!answer.refused());
        assert_eq!(answer.body, big);
    });
}

#[test]
fn injected_secret_of_sixteen_characters_or_more_is_searched_for_in_answer() {
    block_on_local(async {
        let services = production_services();
        for (real, refused) in [("user-real-api-key-123", true), ("short-secret", false)] {
            let secret = InjectedSecret::new(masked("ANTHROPIC_API_KEY", real, "masked"));
            let data = format!("data: {real}\n\n");
            let answer = get_messages(
                &services,
                &[secret],
                streaming(&[&data], "text/event-stream"),
            )
            .await;
            assert_eq!(answer.status, StatusCode::OK, "{real}");
            assert_eq!(answer.failed, refused, "{real}");
            assert_eq!(answer.body.contains(real), !refused, "{real}");
        }
    });
}

#[test]
fn api_answer_with_token_in_header_or_trailer_or_compressed_is_refused() {
    block_on_local(async {
        let services = production_services();
        let token = shaped_token("sk-ant-oat01");
        for (name, value) in [
            ("x-echo", token.clone()),
            ("set-cookie", format!("s={token}; Path=/")),
            ("content-encoding", "gzip".into()),
            ("content-encoding", "br".into()),
        ] {
            let next: Next = Box::new(move |_req| {
                Box::pin(async move {
                    let mut resp: Response<ResponseBody> =
                        Response::new(http_body_util::Either::Right(Bytes::from("x").into()));
                    resp.headers_mut().insert(name, value.parse().unwrap());
                    Ok(resp)
                })
            });
            let answer = get_messages(&services, &[], next).await;
            assert_eq!(answer.status, 502, "{name}");
        }
        for content_type in ["application/json", "text/event-stream"] {
            let mut trailers = HeaderMap::new();
            trailers.insert("x-echo", token.parse().unwrap());
            let frames = vec![
                Frame::data(Bytes::from("data: ok\n\n")),
                Frame::trailers(trailers),
            ];
            let answer = get_messages(&services, &[], streaming_frames(frames, content_type)).await;
            assert!(answer.refused(), "{content_type}");
        }
    });
}

#[test]
fn api_answers_with_real_tokens_are_refused_and_others_pass_intact() {
    block_on_local(async {
        let services = production_services();
        let token = shaped_token("sk-ant-api03");
        for (content_type, body, refused) in [
            ("application/json", format!(r#"{{"key":"{token}"}}"#), true),
            (
                "application/json",
                format!(r#"{{"t":["{}"]}}"#, shaped_token("sk-ant-ort01")),
                true,
            ),
            ("text/plain", shaped_token("sk-ant-oat01"), true),
            (
                "application/json",
                r#"{"key":"sk-ant-api03-airlock-x"}"#.into(),
                false,
            ),
            (
                "application/json",
                r#"{"key":"sk-ant-api03-REAL-short"}"#.into(),
                false,
            ),
            (
                "application/json",
                format!(r#"{{"id":"assert_{token}"}}"#),
                false,
            ),
            (
                "application/json",
                r#"{"access_token":"x","code":"c"}"#.into(),
                false,
            ),
        ] {
            let next = answering(&GotLog::default(), content_type, &body);
            let answer = get_messages(&services, &[], next).await;
            assert_eq!(answer.refused(), refused, "{body}: {}", answer.body);
            if !refused {
                assert_eq!(answer.body, body);
            }
        }
    });
}
