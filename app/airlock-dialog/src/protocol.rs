//! Messages of the dialog.
//!
//! The caller sends one request: a title, labeled values to read and the
//! text of the two buttons. The dialog gives back one answer. The values
//! are plain text. The dialog does not read markup in them.

use serde::{Deserialize, Serialize};

/// Request to show.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// Window title.
    pub title: String,
    /// Values to read, in the order to show.
    pub items: Vec<Item>,
    /// Label of the allow button.
    pub allow: String,
    /// Label of the deny button.
    pub deny: String,
    /// Label of the countdown text. The dialog adds the seconds left.
    pub countdown: String,
    /// Time limit in milliseconds. At the end the answer is a timeout.
    pub timeout_ms: u64,
}

/// One labeled value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Item {
    /// Label of the value.
    pub label: String,
    /// Value. It can have more lines.
    pub value: String,
    /// Lines of a change. If set, the dialog shows these lines in color in
    /// place of the value.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<Vec<DiffLine>>,
}

/// One line of a change.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffLine {
    /// Change kind of the line.
    pub kind: DiffKind,
    /// Line text, without the change mark.
    pub text: String,
}

/// Change kind of a line.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DiffKind {
    /// The change adds the line.
    Add,
    /// The change removes the line.
    Remove,
    /// The change keeps the line.
    Keep,
}

/// Answer of the user.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Answer {
    /// The user selected the allow button.
    Allow,
    /// The user selected the deny button, pressed Escape or closed the
    /// window.
    Deny,
    /// The time limit ended.
    Timeout,
}

/// Error that stops the dialog, for example when there is no X server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Error(pub String);

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

// Error does not implement the error trait itself. Else this conversion
// would conflict with the identity conversion.
impl<E: std::error::Error> From<E> for Error {
    fn from(e: E) -> Self {
        Self(e.to_string())
    }
}
