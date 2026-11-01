//! Messages between airlock and the macOS notifier app.
//!
//! Airlock sends the prompt text and the time limit to the app. The app
//! sends back one answer: allow, deny, dismissed, timeout or error. The
//! answer can also tell the alert style that the user set for the app, and
//! that the app must start through the system app launcher to ask for the
//! notification permission. After an allow answer the app can show a
//! notice while the command runs, and tell airlock when the user aborts.

// Only macOS starts the app. The tests check the messages on all hosts.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use serde::{Deserialize, Serialize};

use crate::approval::Decision;
pub(super) use crate::approval::text::DetailItem;
use crate::messages::grants as msg;

/// Request from airlock to the app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ChildRequest {
    /// Notification title.
    pub title: String,
    /// Notification text.
    pub body: String,
    /// Time limit in milliseconds. The app stops when it ends.
    pub timeout_ms: u64,
    /// Notice to show after an allow answer, until airlock closes the
    /// input of the app.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running: Option<RunningNotice>,
    /// All details of the request. The app shows them in a window when the
    /// user asks for them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub details: Option<ChildDetails>,
}

/// Details of a request for the details window.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ChildDetails {
    /// Window title.
    pub title: String,
    /// Label of the notification action that opens the window.
    pub more: String,
    /// Labeled values, in order.
    pub items: Vec<DetailItem>,
}

/// Notice that shows while an approved command runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct RunningNotice {
    /// Notification title.
    pub title: String,
    /// Notification text.
    pub body: String,
    /// Label of the abort button.
    pub abort: String,
}

/// Event from the app after the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub(super) enum ChildEvent {
    /// The user selected abort on the running notice.
    Abort,
}

/// Answer kind of the app.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum ChildDecision {
    /// The user selected allow.
    Allow,
    /// The user selected deny.
    Deny,
    /// The notification closed without a button.
    Dismissed,
    /// The time limit ended.
    Timeout,
    /// The notification could not show.
    Error,
}

/// Alert style that the user set for the app in the system settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(super) enum AlertStyle {
    /// The notification does not show on the screen.
    None,
    /// The notification shows for a short time.
    Banner,
    /// The notification stays on the screen until the user closes it.
    Alert,
}

/// Answer of the app.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct ChildAnswer {
    /// Answer kind.
    pub decision: ChildDecision,
    /// Error text for `Error`.
    #[serde(default)]
    pub detail: String,
    /// Alert style of the app, if the app could read it.
    #[serde(default)]
    pub alert_style: Option<AlertStyle>,
    /// True with `Error` when the user did not decide on the notification
    /// permission yet.
    #[serde(default)]
    pub needs_open: bool,
}

/// Result of one start of the app.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) enum Reply {
    /// The user decided, or the time limit ended.
    Decision(Decision),
    /// The app must start again through the system app launcher, so that
    /// the system can ask for the permission. The text tells why.
    NeedsOpen(String),
    /// The notification could not show. A fallback must ask.
    Error(String),
}

impl ChildAnswer {
    /// Get the result of the answer.
    pub(super) fn reply(self) -> Reply {
        match self.decision {
            ChildDecision::Allow => Reply::Decision(Decision::Allow),
            ChildDecision::Deny => Reply::Decision(Decision::DeniedByUser),
            ChildDecision::Dismissed => Reply::Decision(Decision::Deny(msg::DISMISSED.into())),
            ChildDecision::Timeout => Reply::Decision(Decision::Deny(msg::TIMEOUT.into())),
            ChildDecision::Error if self.needs_open => Reply::NeedsOpen(self.detail),
            ChildDecision::Error => Reply::Error(self.detail),
        }
    }

    /// Return `true` if the notification goes away before the user can
    /// answer: the alert style is banner or none.
    pub(super) fn needs_alert_hint(&self) -> bool {
        matches!(
            self.alert_style,
            Some(AlertStyle::Banner | AlertStyle::None)
        )
    }
}

/// Maximum size of the request line, in bytes. The app refuses a line
/// longer than 64 KiB.
const MAX_REQUEST: usize = 60 * 1024;

/// Maximum length of one detail value when the request is too long, in
/// characters.
const SHORT_DETAIL: usize = 1000;

/// Make the request line for the app, with the end of line. When the line
/// is too long, the detail values are cut and the diffs show as plain text.
/// If it is still too long, the details go away.
pub(super) fn encode_request(req: &ChildRequest) -> Result<Vec<u8>, String> {
    let encode = |req: &ChildRequest| -> Result<Vec<u8>, String> {
        let mut line = serde_json::to_vec(req).map_err(|e| e.to_string())?;
        line.push(b'\n');
        Ok(line)
    };
    let line = encode(req)?;
    if line.len() <= MAX_REQUEST || req.details.is_none() {
        return Ok(line);
    }
    let mut short = req.clone();
    if let Some(details) = &mut short.details {
        for item in &mut details.items {
            item.value =
                crate::approval::text::cut_end(std::mem::take(&mut item.value), SHORT_DETAIL);
            // The plain text form of a diff stays.
            item.diff = None;
        }
    }
    let line = encode(&short)?;
    if line.len() <= MAX_REQUEST {
        return Ok(line);
    }
    short.details = None;
    encode(&short)
}

/// Read the answer from the output of the app.
/// Args:
///  - `stdout`: Output of the app. Only the first line counts.
///
/// Returns:
///   The answer, or error text when the output is not an answer.
pub(super) fn parse_answer(stdout: &[u8]) -> Result<ChildAnswer, String> {
    let line = stdout.split(|b| *b == b'\n').next().unwrap_or_default();
    serde_json::from_slice(line).map_err(|e| e.to_string())
}

/// Read one event line of the app.
///
/// Returns:
///   The event, or `None` when the line is not an event.
pub(super) fn parse_event(line: &[u8]) -> Option<ChildEvent> {
    serde_json::from_slice(line.trim_ascii_end()).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that a request goes through the line format without change. A
    /// request without the running notice keeps the old format.
    ///   1. Encode a request with new lines and quotes in the text
    ///   2. Check that it is one line, has no running notice field and
    ///      parses back the same
    ///   3. Add a running notice and check the same again
    #[test]
    fn request_line_round_trip_keeps_text() {
        let req = ChildRequest {
            title: "airlock: p asks".into(),
            body: "Reason: \"x\"\nCommand: y".into(),
            timeout_ms: 1500,
            running: None,
            details: None,
        };
        let line = String::from_utf8(encode_request(&req).unwrap()).unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        assert!(!line.contains("running"));
        assert_eq!(serde_json::from_str::<ChildRequest>(&line).unwrap(), req);
        let req = ChildRequest {
            running: Some(RunningNotice {
                title: "active".into(),
                body: "Running...\nRule: npm".into(),
                abort: "Abort".into(),
            }),
            ..req
        };
        let line = String::from_utf8(encode_request(&req).unwrap()).unwrap();
        assert_eq!(line.matches('\n').count(), 1);
        assert_eq!(serde_json::from_str::<ChildRequest>(&line).unwrap(), req);
    }

    /// Test that the details go to the app as labeled values, and that a
    /// request that is too long for the app gets shorter values.
    ///   1. Encode a request with details and check that it parses back
    ///      the same
    ///   2. Encode a request with a very long detail value
    ///   3. Check that the line fits the limit and the value is cut
    #[test]
    fn request_details_round_trip_and_long_values_are_cut() {
        let item = |value: &str| DetailItem {
            label: "Command".into(),
            value: value.into(),
            mono: true,
            diff: None,
        };
        let req = ChildRequest {
            title: "t".into(),
            body: "b".into(),
            timeout_ms: 5,
            running: None,
            details: Some(ChildDetails {
                title: "Network access requested".into(),
                more: "More details".into(),
                items: vec![item("npm install\nnpm test")],
            }),
        };
        let line = encode_request(&req).unwrap();
        assert!(String::from_utf8_lossy(&line).contains(r#""label":"Command""#));
        assert_eq!(serde_json::from_slice::<ChildRequest>(&line).unwrap(), req);

        let long = ChildRequest {
            details: Some(ChildDetails {
                items: vec![item(&"x".repeat(100_000))],
                ..req.details.clone().unwrap()
            }),
            ..req
        };
        let line = encode_request(&long).unwrap();
        assert!(line.len() <= MAX_REQUEST);
        let back: ChildRequest = serde_json::from_slice(&line).unwrap();
        assert_eq!(
            back.details.unwrap().items[0].value.chars().count(),
            SHORT_DETAIL
        );
    }

    /// Test that the abort line of the app parses as the abort event, and
    /// that other lines are not events.
    ///   1. Parse the abort line with and without the end of line
    ///   2. Check that empty, unknown and bad lines give no event
    #[test]
    fn abort_line_parses_as_abort_event() {
        assert_eq!(
            parse_event(b"{\"event\":\"abort\"}\n"),
            Some(ChildEvent::Abort)
        );
        assert_eq!(
            parse_event(br#"{"event":"abort"}"#),
            Some(ChildEvent::Abort)
        );
        assert_eq!(parse_event(b""), None);
        assert_eq!(parse_event(br#"{"event":"other"}"#), None);
        assert_eq!(parse_event(br#"{"decision":"allow"}"#), None);
    }

    /// Test that each answer line of the notifier app gives the correct
    /// result. An error asks for the fallback, or for a start through the
    /// app launcher when the permission is not decided.
    ///   1. Parse the answer line of each answer kind
    ///   2. Check the decision, or the error kind and text
    ///   3. Check that empty and bad output are errors
    #[test]
    fn notifier_answers_map_to_decisions_and_errors() {
        let reply =
            |line: &str| parse_answer(format!("{line}\nmore").as_bytes()).map(ChildAnswer::reply);
        let decided = |d| Ok(Reply::Decision(d));
        assert_eq!(
            reply(r#"{"decision":"allow","detail":""}"#),
            decided(Decision::Allow)
        );
        assert_eq!(
            reply(r#"{"decision":"deny"}"#),
            decided(Decision::DeniedByUser)
        );
        assert_eq!(
            reply(r#"{"decision":"dismissed","alert_style":"alert"}"#),
            decided(Decision::Deny(msg::DISMISSED.into()))
        );
        assert_eq!(
            reply(r#"{"decision":"timeout","detail":""}"#),
            decided(Decision::Deny(msg::TIMEOUT.into()))
        );
        assert_eq!(
            reply(r#"{"decision":"error","detail":"no app"}"#),
            Ok(Reply::Error("no app".into()))
        );
        assert_eq!(
            reply(r#"{"decision":"error","detail":"not allowed","needs_open":true}"#),
            Ok(Reply::NeedsOpen("not allowed".into()))
        );
        assert!(parse_answer(b"").is_err());
        assert!(parse_answer(br#"{"decision":"maybe"}"#).is_err());
    }

    /// Test that only the banner and none alert styles ask for the hint.
    /// With these styles the request can go away before the user answers.
    ///   1. Parse answers with each alert style and without one
    ///   2. Check the hint flag
    #[test]
    fn banner_and_none_alert_styles_ask_for_hint() {
        let hint = |style: &str| {
            parse_answer(format!(r#"{{"decision":"allow"{style}}}"#).as_bytes())
                .unwrap()
                .needs_alert_hint()
        };
        assert!(hint(r#","alert_style":"banner""#));
        assert!(hint(r#","alert_style":"none""#));
        assert!(!hint(r#","alert_style":"alert""#));
        assert!(!hint(""));
    }
}
