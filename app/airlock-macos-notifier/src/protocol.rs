//! Messages between airlock and the helper app.
//!
//! Airlock sends the notification text and the time limit. The helper sends
//! back one answer: allow, deny, dismissed, timeout or error. The answer
//! also tells the alert style that the user set for the app, and if the
//! app must start one time through the system app launcher to ask for the
//! notification permission. After an allow answer the helper can show a
//! notice while the command runs, and send one abort event line. The
//! request can also give details that the helper shows in a window when
//! the user asks for them.

// Only macOS shows the notification. The tests check the messages on all
// hosts.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use serde::{Deserialize, Serialize};

/// Maximum size of one request line, in bytes.
pub(crate) const MAX_REQUEST: u64 = 64 * 1024;

/// Request from airlock.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct ChildRequest {
    /// Notification title.
    pub title: String,
    /// Notification text.
    pub body: String,
    /// Time limit in milliseconds. The helper stops when it ends.
    pub timeout_ms: u64,
    /// Notice to show after an allow answer, until airlock closes stdin.
    #[serde(default)]
    pub running: Option<RunningNotice>,
    /// Details to show in a window when the user asks for them.
    #[serde(default)]
    pub details: Option<Details>,
}

/// Details of the request. The values are plain text. The helper does not
/// read markup in them.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct Details {
    /// Window title.
    pub title: String,
    /// Label of the button that opens the window.
    pub more: String,
    /// Items in the window, in the order to show.
    pub items: Vec<DetailItem>,
}

/// One item of the details: a label and a value below it.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct DetailItem {
    /// Item label.
    pub label: String,
    /// Item value. It can have more than one line.
    pub value: String,
    /// True if the value shows in a monospaced font.
    #[serde(default)]
    pub mono: bool,
    /// Lines of a change. If set, the window shows these lines in color in
    /// place of the value.
    #[serde(default)]
    pub diff: Option<Vec<DiffLine>>,
}

/// One line of a change.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct DiffLine {
    /// Change kind of the line.
    pub kind: DiffKind,
    /// Line text, without the change mark.
    pub text: String,
}

/// Change kind of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum DiffKind {
    /// The change adds the line.
    Add,
    /// The change removes the line.
    Remove,
    /// The line does not change.
    Same,
}

/// Notice that shows while an approved command runs.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
pub(crate) struct RunningNotice {
    /// Notification title.
    pub title: String,
    /// Notification text.
    pub body: String,
    /// Label of the abort button.
    pub abort: String,
}

/// Event line after the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub(crate) enum ChildEvent {
    /// The user selected abort on the running notice.
    Abort,
}

impl ChildEvent {
    /// Make the event line, without the end of line.
    pub(crate) fn encode(self) -> String {
        serde_json::to_string(&self).unwrap_or_default()
    }
}

/// Answer kind of the helper.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ChildDecision {
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
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum AlertStyle {
    /// The notification does not show on the screen.
    None,
    /// The notification shows for a short time.
    Banner,
    /// The notification stays on the screen until the user closes it.
    Alert,
}

/// Answer of the helper.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub(crate) struct ChildAnswer {
    /// Answer kind.
    pub decision: ChildDecision,
    /// Error text for `Error`.
    pub detail: String,
    /// Alert style of the app, if the helper could read it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub alert_style: Option<AlertStyle>,
    /// True with `Error` when the user did not decide on the permission
    /// yet. The system shows the permission prompt only when the app
    /// starts through the system app launcher.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub needs_open: bool,
}

impl ChildAnswer {
    /// Make an answer without the alert style.
    pub(crate) fn new(decision: ChildDecision, detail: &str) -> Self {
        Self {
            decision,
            detail: detail.to_owned(),
            alert_style: None,
            needs_open: false,
        }
    }

    /// Make the answer line, without the end of line.
    pub(crate) fn encode(&self) -> String {
        // The answer has only plain strings, so it always serializes.
        serde_json::to_string(self).unwrap_or_default()
    }
}

/// Read the request line.
pub(crate) fn parse_request(line: &str) -> Result<ChildRequest, String> {
    serde_json::from_str(line.trim_end()).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test that the answer line has the optional fields only when they
    /// are set. Airlock reads an answer without them as before.
    ///   1. Encode a plain answer and check that it has only two fields
    ///   2. Encode an error answer with the alert style and the open flag
    ///   3. Check the field values
    #[test]
    fn answer_line_has_optional_fields_only_when_set() {
        assert_eq!(
            ChildAnswer::new(ChildDecision::Allow, "").encode(),
            r#"{"decision":"allow","detail":""}"#
        );
        let answer = ChildAnswer {
            alert_style: Some(AlertStyle::Banner),
            needs_open: true,
            ..ChildAnswer::new(ChildDecision::Error, "not allowed")
        };
        assert_eq!(
            answer.encode(),
            r#"{"decision":"error","detail":"not allowed","alert_style":"banner","needs_open":true}"#
        );
    }

    /// Test the running notice and the abort event lines that airlock
    /// reads and writes.
    ///   1. Parse a request without and with the running notice
    ///   2. Encode the abort event and check the line
    #[test]
    fn running_notice_and_abort_event_lines() {
        let req = parse_request(r#"{"title":"t","body":"b","timeout_ms":5}"#).unwrap();
        assert_eq!(req.running, None);
        let req = parse_request(
            r#"{"title":"t","body":"b","timeout_ms":5,"running":{"title":"rt","body":"rb","abort":"ab"}}"#,
        )
        .unwrap();
        assert_eq!(
            req.running,
            Some(RunningNotice {
                title: "rt".into(),
                body: "rb".into(),
                abort: "ab".into(),
            })
        );
        assert_eq!(ChildEvent::Abort.encode(), r#"{"event":"abort"}"#);
    }

    /// Test that the request gives the details for the window, and that a
    /// request without details has none. Airlock sends the details only
    /// when it has them.
    ///   1. Parse a request without details and check that it has none
    ///   2. Parse a request with items with and without the mono flag
    ///   3. Check the item values and the mono flags
    #[test]
    fn request_details_parse_with_and_without_mono() {
        let req = parse_request(r#"{"title":"t","body":"b","timeout_ms":5}"#).unwrap();
        assert_eq!(req.details, None);
        let req = parse_request(
            r#"{"title":"t","body":"b","timeout_ms":5,"details":{"title":"dt","more":"More",
            "items":[{"label":"Sandbox","value":"p"},{"label":"Env","value":"A=1\nB=2","mono":true}]}}"#,
        )
        .unwrap();
        assert_eq!(
            req.details,
            Some(Details {
                title: "dt".into(),
                more: "More".into(),
                items: vec![
                    DetailItem {
                        label: "Sandbox".into(),
                        value: "p".into(),
                        mono: false,
                        diff: None,
                    },
                    DetailItem {
                        label: "Env".into(),
                        value: "A=1\nB=2".into(),
                        mono: true,
                        diff: None,
                    },
                ],
            })
        );
    }

    /// Test that an item can give the lines of a change with all change
    /// kinds, and that an item without them has none. The window shows the
    /// lines in place of the value.
    ///   1. Parse a request with one item with change lines and one without
    ///   2. Check the change lines and their kinds
    ///   3. Check that the second item has no change lines
    #[test]
    fn detail_item_diff_parses_all_kinds_and_defaults_to_none() {
        let req = parse_request(
            r#"{"title":"t","body":"b","timeout_ms":5,"details":{"title":"dt","more":"More",
            "items":[{"label":"Rule","value":"+ [network.rules.npm]","mono":true,
            "diff":[{"kind":"add","text":"[network.rules.npm]"},{"kind":"same","text":"enabled = true"},
            {"kind":"remove","text":"deny = []"}]},{"label":"Sandbox","value":"p"}]}}"#,
        )
        .unwrap();
        let items = req.details.unwrap().items;
        let line = |kind, text: &str| DiffLine {
            kind,
            text: text.into(),
        };
        assert_eq!(
            items[0].diff,
            Some(vec![
                line(DiffKind::Add, "[network.rules.npm]"),
                line(DiffKind::Same, "enabled = true"),
                line(DiffKind::Remove, "deny = []"),
            ])
        );
        assert_eq!(items[1].diff, None);
    }
}
