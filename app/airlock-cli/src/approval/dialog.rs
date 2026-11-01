//! Details dialog for the approval on Linux.
//!
//! Shows all details of the request in a window with allow and deny
//! buttons. The window needs an X server (also the X compatibility layer of
//! a Wayland desktop). The dialog runs in a child process of the airlock
//! executable, so that the window loop does not block the sandbox and a
//! failure of it cannot stop airlock. A closed window, an error and a
//! time out all mean deny.

use std::io::{Read, Write};
use std::process::Stdio;
use std::time::Duration;

use airlock_dialog::{Answer, DiffKind, DiffLine, Item, Request};
use tokio::io::AsyncWriteExt;

use super::Decision;
use super::text::{self, PromptText};
use crate::messages::grants as msg;

/// Extra wait time for the dialog process after its own time limit.
const GRACE: Duration = Duration::from_secs(5);

/// Maximum size of the answer of the dialog process, in bytes.
const MAX_ANSWER: usize = 4096;

/// Maximum size of the request that the dialog process reads, in bytes.
/// The prompt text has limits on all of its parts, so a real request is
/// much smaller.
const MAX_REQUEST: u64 = 1024 * 1024;

/// Maximum length of the error text of the dialog process, in characters.
const MAX_ERROR: usize = 300;

/// Program that starts the dialog process. It is this executable also
/// when its file was replaced or removed after the start.
const SELF_EXE: &str = "/proc/self/exe";

/// Return `true` if the host has an X display for the dialog.
pub(super) fn available() -> bool {
    std::env::var_os("DISPLAY").is_some_and(|v| !v.is_empty())
}

/// Make the request for the dialog from the prompt text.
fn request(text: &PromptText, timeout: Duration) -> Request {
    let items = text
        .details
        .iter()
        .map(|item| {
            let diff: Option<Vec<DiffLine>> = item
                .diff
                .as_ref()
                .map(|lines| lines.iter().map(diff_line).collect());
            Item {
                label: item.label.clone(),
                // The dialog shows the diff in place of the value. Do not
                // send the same text two times.
                value: if diff.is_some() {
                    String::new()
                } else {
                    item.value.clone()
                },
                diff,
            }
        })
        .collect();
    Request {
        title: text.title.clone(),
        items,
        allow: msg::ALLOW.into(),
        deny: msg::DENY.into(),
        countdown: msg::COUNTDOWN.into(),
        timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
    }
}

fn diff_line(line: &text::DiffLine) -> DiffLine {
    DiffLine {
        kind: match line.kind {
            text::DiffKind::Add => DiffKind::Add,
            text::DiffKind::Remove => DiffKind::Remove,
            text::DiffKind::Same => DiffKind::Keep,
        },
        text: line.text.clone(),
    }
}

/// Show the request in a window and wait for the answer.
/// Args:
///  - `text`: Clean prompt text
///  - `timeout`: Maximum wait time
///
/// Returns:
///   Decision of the user, also deny on timeout. Error text when the
///   window cannot show.
pub(super) async fn ask(text: &PromptText, timeout: Duration) -> Result<Decision, String> {
    let input = serde_json::to_vec(&request(text, timeout)).map_err(|e| e.to_string())?;
    match run(input, timeout).await? {
        Answer::Allow => Ok(Decision::Allow),
        Answer::Deny => Ok(Decision::DeniedByUser),
        Answer::Timeout => Ok(Decision::Deny(msg::TIMEOUT.into())),
    }
}

/// Run the dialog process.
/// Args:
///  - `input`: Request as JSON
///  - `timeout`: Time limit of the request
async fn run(input: Vec<u8>, timeout: Duration) -> Result<Answer, String> {
    let mut child = tokio::process::Command::new(SELF_EXE)
        .arg(msg::DIALOG_ARG)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The window closes with the wait, so a late answer has no effect.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let mut stdin = child.stdin.take().ok_or("no input of the dialog process")?;
    let wait = async {
        // The process can end before it reads the request. The exit status
        // tells the result then.
        let _ = stdin.write_all(&input).await;
        drop(stdin);
        child.wait_with_output().await
    };
    let Ok(out) = tokio::time::timeout(timeout + GRACE, wait).await else {
        return Ok(Answer::Timeout);
    };
    let out = out.map_err(|e| e.to_string())?;
    if !out.status.success() {
        // The text can come from the X server. Clean it like other
        // text that the user sees.
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(text::cut_end(text::clean(&err), MAX_ERROR));
    }
    serde_json::from_slice(&out.stdout[..out.stdout.len().min(MAX_ANSWER)])
        .map_err(|e| e.to_string())
}

/// Run the dialog process: read one request from standard input, show the
/// dialog and write the answer to standard output.
///
/// Returns:
///   Exit code of the process. The error text goes to standard error.
pub(crate) fn child_main() -> i32 {
    let mut input = Vec::new();
    let read = std::io::stdin()
        .take(MAX_REQUEST)
        .read_to_end(&mut input)
        .map_err(|e| e.to_string())
        .and_then(|_| serde_json::from_slice::<Request>(&input).map_err(|e| e.to_string()));
    let answer = read.and_then(|request| airlock_dialog::show(request).map_err(|e| e.to_string()));
    match answer {
        Ok(answer) => {
            let json = serde_json::to_string(&answer).unwrap_or_default();
            let _ = writeln!(std::io::stdout(), "{json}");
            0
        }
        Err(e) => {
            let _ = writeln!(std::io::stderr(), "{e}");
            1
        }
    }
}
