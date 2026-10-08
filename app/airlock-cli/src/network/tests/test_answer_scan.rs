//! The scan of API answers: real tokens and injected secrets must not get
//! to the guest in a body, a header or a trailer, also in streamed answers.

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

/// Send `GET /v1/messages` without a credential to the production Anthropic
/// API host, with `injected` secrets and `next` as the upstream.
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

/// Test that the proxy scans API answers as they stream, also when a token
/// is split over chunks. Answers without a real token must pass intact.
///   1. Store a grant with a real refresh token
///   2. Stream answers with a real token split over chunks, in JSON and in
///      an event stream, and check that they are refused
///   3. Stream model text with a token shape and many surrogates, and check
///      that they pass intact
///   4. Stream a large JSON answer with a token at the end and check that the
///      stream ends with an error before the token
///   5. Send a large JSON answer without a token and check that it passes
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
        // A JSON answer below the hold limit: the proxy holds it and answers
        // with a local 502.
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

        // An event stream goes to the guest at once. The proxy ends the
        // stream with an error just before the stored real token.
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

        // In an event stream (model text) only stored real values count, not
        // the token shapes.
        let model = format!("data: {{\"text\":\"an example key: {token}\"}}\n\n");
        let answer = get_messages(&services, &[], streaming(&[&model], "text/event-stream")).await;
        assert!(!answer.refused());
        assert_eq!(answer.body, model);

        // Many surrogate-shaped values in a long event stream.
        let line = format!(
            "data: {{\"k\":\"sk-ant-oat01-airlock-{}\"}}\n\n",
            "x".repeat(50)
        );
        let chunks: Vec<String> = (0..300).map(|_| line.clone()).collect();
        let refs: Vec<&str> = chunks.iter().map(String::as_str).collect();
        let answer = get_messages(&services, &[], streaming(&refs, "text/event-stream")).await;
        assert!(!answer.refused());
        assert_eq!(answer.body, chunks.concat());

        // 100 KiB of padding is more than the JSON hold limit. Thus the status
        // and the padding go to the guest before the scan finds the token.
        // The spaces end each token run, so the padding is no token candidate.
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

        // One token run of 100 KiB without a token prefix passes intact.
        let big = format!(r#"{{"pad":"{}"}}"#, "a".repeat(100 * 1024));
        let next = answering(&GotLog::default(), "application/json", &big);
        let answer = get_messages(&services, &[], next).await;
        assert!(!answer.refused());
        assert_eq!(answer.body, big);
    });
}

/// Test that the scan searches for an injected secret only if it has 16 or
/// more characters. A short secret can match model text by chance and cut
/// the answer.
///   1. Inject a long secret and a short secret
///   2. Stream an answer that contains the real value
///   3. Check that the stream with the long secret ends with an error and
///      the stream with the short secret passes
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

/// Test that an API answer is refused if a header or a trailer has a real
/// token, or if the answer is compressed. The scan cannot read compressed
/// bodies.
///   1. Send answers with a token in a header, in `Set-Cookie`, or with gzip
///      or brotli encoding, and check the local 502
///   2. Stream JSON and event-stream answers with a token in a trailer
///   3. Check that the trailer answers are refused
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

/// Test that API answers with a real token shape are refused, and that
/// lookalikes pass intact.
///   1. Send answers with real-shaped API, refresh and access tokens and
///      check that they are refused
///   2. Send answers with a surrogate, a short fake, a token after a prefix,
///      and token field names, and check that they pass without change
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

/// Test that the scan also checks the answer to a request with a
/// credential. The proxy puts the real token in that request, so an
/// upstream that echoes the request can send the real token back.
///   1. Store a grant with a real access token
///   2. Send an API request with the access surrogate as the credential
///   3. Let the upstream stream the real token back, split over two chunks
///   4. Check that the upstream got the real token and that the answer is
///      refused without the token
#[test]
fn answer_to_request_with_credential_is_scanned() {
    block_on_local(async {
        let services = production_services();
        let real = shaped_token("sk-ant-oat01");
        let surrogate = "sk-ant-oat01-airlock-A";
        insert_grant(
            &services.store,
            ServiceId::Anthropic,
            &[(TokenKind::Access, &real, surrogate)],
        )
        .await;
        let got = std::rc::Rc::new(std::cell::RefCell::new(String::new()));
        let seen = got.clone();
        let (first, second) = real.split_at(30);
        let echo = streaming(
            &[&format!("data: {first}"), &format!("{second}\n\n")],
            "text/event-stream",
        );
        let next: Next = Box::new(move |req| {
            *seen.borrow_mut() = req.headers()["authorization"].to_str().unwrap().into();
            echo(req)
        });
        let bearer = format!("Bearer {surrogate}");
        let req = request("GET", "/v1/messages", &[("authorization", &bearer)], "");
        let answer = services
            .send(ServiceId::Anthropic, "api.anthropic.com", req, &[], next)
            .await;
        assert_eq!(*got.borrow(), format!("Bearer {real}"));
        assert!(answer.refused(), "{}", answer.body);
        assert!(!answer.body.contains(&real), "{}", answer.body);
    });
}
