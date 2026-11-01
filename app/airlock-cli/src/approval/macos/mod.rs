//! Approval through Notification Center, with a system dialog as fallback.
//!
//! The notification shows allow, deny and details buttons. The details
//! button and a click on the notification open a window with all details
//! of the request. Notification Center
//! accepts notifications only from an app bundle, so a small notifier app
//! shows it. The airlock binary contains the app and installs it to the
//! data directory on first use, and again when the app version changes. Before the user decides on the notification
//! permission, the system asks for it only when the app starts through the
//! system app launcher. When the notification cannot show, a system dialog
//! asks the user. The request text goes to the dialog script as arguments,
//! so that the text cannot change the script. When the dialog also cannot
//! show, the request goes to the next way to ask.
//!
//! After the user allows a command, the notifier app can keep a notice
//! with an abort button while the command runs. Only a direct start of the
//! app supports the notice.

mod notifier;
mod protocol;

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::time::Instant;

use self::notifier::Installed;
use self::protocol::{ChildDecision, ChildEvent, ChildRequest, Reply, RunningNotice};
use super::text::PromptText;
use super::{Approval, Asked, Decision, Running, RunningControl};
use crate::data_dir::DataDir;
use crate::messages::grants as msg;
use crate::util::VerifiedPath;

/// Archive of the notifier app bundle. Empty when the build did not have
/// the app.
const NOTIFIER_TAR: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/notifier.tar"));

/// Absolute path of the script runner, so that `PATH` cannot change it.
const OSASCRIPT: &str = "/usr/bin/osascript";

/// Notifier directory of older airlock versions in the data directory.
const LEGACY_DIR: &str = "notifier";

/// Absolute path of the system app launcher.
const OPEN: &str = "/usr/bin/open";

/// Absolute path of the tool that registers apps with the system.
const LSREGISTER: &str = "/System/Library/Frameworks/CoreServices.framework/Frameworks/\
                          LaunchServices.framework/Support/lsregister";

/// Maximum size of the answer and the diagnostics of the notifier app, in
/// bytes.
const MAX_OUTPUT: u64 = 64 * 1024;

/// Extra wait time for the dialog process after the dialog gives up.
const GRACE: Duration = Duration::from_secs(5);

/// Time between the end of the time limit of the notifier app and the
/// deadline of airlock. In this time the app removes the notification and
/// writes the timeout answer.
const APP_MARGIN: Duration = Duration::from_secs(2);

/// Wait time for the answer of the notifier app after airlock closes its
/// input.
const APP_GRACE: Duration = Duration::from_secs(2);

/// Dialog script. Arguments: title, body, allow label, deny label, timeout
/// in seconds. Prints the label of the selected button, or nothing on
/// timeout. Deny is the default button, so that the Return key denies.
const SCRIPT: &str = r#"on run argv
set {t, b, okLabel, noLabel, secs} to argv
set r to display dialog b with title t buttons {noLabel, okLabel} default button noLabel with icon caution giving up after (secs as integer)
if gave up of r then return ""
return button returned of r
end run"#;

/// Show the request and wait for the answer. Tries the notification
/// first, then the dialog.
/// Args:
///  - `text`: Clean prompt text
///  - `timeout`: Maximum wait time
///  - `data`: Data directory. The notifier app installs there.
///
/// Returns:
///   Decision of the user, and the running notice if it shows. Deny on
///   timeout. Unavailable when nothing can show.
pub(super) async fn ask(text: PromptText, timeout: Duration, data: &DataDir) -> Asked {
    let deadline = Instant::now() + timeout;
    match notify(&text, deadline, data).await {
        Ok(approval) => Asked::Shown(approval),
        Err(e) => {
            tracing::warn!("grants: notification failed, using a dialog: {e}");
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Asked::Shown(Decision::Deny(msg::TIMEOUT.into()).into());
            }
            match dialog(&text, left).await {
                Ok(decision) => Asked::Shown(decision.into()),
                Err(why) => Asked::Unavailable(why),
            }
        }
    }
}

/// Show the request in Notification Center through the notifier app.
///
/// Returns:
///   Decision of the user and the running notice, or error text when the
///   notification cannot show. Timeout is a decision (deny), not an error.
async fn notify(text: &PromptText, deadline: Instant, data: &DataDir) -> Result<Approval, String> {
    let installed = install(data.clone()).await.map_err(|e| format!("{e:#}"))?;
    let mut reply = match run_direct(&installed.app, text, deadline).await? {
        Direct::Running(running) => {
            return Ok(Approval {
                decision: Decision::Allow,
                running: Some(running),
            });
        }
        Direct::Reply(reply) => reply,
    };
    if let Reply::NeedsOpen(why) = &reply {
        tracing::info!("grants: notifier app starts through the app launcher: {why}");
        reply = run_open(&installed.app, text, deadline, &data.support).await?;
    }
    match reply {
        Reply::Decision(decision) => Ok(decision.into()),
        Reply::NeedsOpen(e) | Reply::Error(e) => Err(e),
    }
}

/// Install the notifier app and register a new install with the system.
/// Remove the notifier directory of older airlock versions once per
/// process.
async fn install(data: DataDir) -> anyhow::Result<Installed> {
    static FIRST: AtomicBool = AtomicBool::new(true);
    if NOTIFIER_TAR.is_empty() {
        anyhow::bail!(msg::NOTIFIER_NOT_BUNDLED);
    }
    // The unpack, the lock wait and the registration tool block. Keep them
    // off the event loop of the sandbox.
    tokio::task::spawn_blocking(move || {
        let installed = notifier::install(&data.support, NOTIFIER_TAR)?;
        let first = FIRST.swap(false, Ordering::Relaxed);
        if installed.fresh {
            tracing::debug!("grants: notifier app installed: {}", installed.app);
        }
        // The system reads the name and the icon of the app at the
        // registration. Without it, the first notification can show
        // without them. Register also an app that is already there once
        // per process: a crash after the install can skip the
        // registration.
        if installed.fresh || first {
            lsregister("-f", &installed.app);
        }
        if first && let Ok(old) = data.root.join(LEGACY_DIR) {
            // Remove the registration, so that the system keeps no record
            // of a removed app.
            notifier::remove_legacy(&old, |app| lsregister("-u", app));
        }
        Ok(installed)
    })
    .await?
}

/// Run the registration tool for the app. Best effort: errors only go to
/// the log.
fn lsregister(op: &str, app: &VerifiedPath) {
    let Ok(path) = app.to_path() else {
        return;
    };
    let status = std::process::Command::new(LSREGISTER)
        .arg(op)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    if !status.as_ref().is_ok_and(std::process::ExitStatus::success) {
        tracing::debug!("grants: lsregister {op} failed: {status:?}");
    }
}

/// Make the request for the notifier app. The time limit of the app ends
/// a short time before `deadline`, so that the app can remove the
/// notification and answer before airlock stops it.
/// Args:
///  - `running`: True to ask for the running notice, if the text has one
///
/// Returns:
///   The request line, or `None` when not enough time is left.
fn request(text: &PromptText, deadline: Instant, running: bool) -> Result<Option<Vec<u8>>, String> {
    let left = deadline.saturating_duration_since(Instant::now());
    let Some(limit) = left.checked_sub(APP_MARGIN).filter(|l| !l.is_zero()) else {
        return Ok(None);
    };
    protocol::encode_request(&ChildRequest {
        title: text.title.clone(),
        body: text.body.clone(),
        timeout_ms: limit.as_millis().try_into().unwrap_or(u64::MAX),
        running: text
            .running
            .clone()
            .filter(|_| running)
            .map(|body| RunningNotice {
                title: msg::RUNNING_TITLE.into(),
                body,
                abort: msg::ABORT.into(),
            }),
        details: Some(protocol::ChildDetails {
            title: text.title.clone(),
            more: msg::MORE_DETAILS.into(),
            items: text.details.clone(),
        }),
    })
    .map(Some)
}

/// Result when the time limit ended.
fn timeout() -> Reply {
    Reply::Decision(Decision::Deny(msg::TIMEOUT.into()))
}

/// Read the output of the app, up to [`MAX_OUTPUT`] bytes.
async fn read_capped(pipe: Option<impl tokio::io::AsyncRead + Unpin>) -> Vec<u8> {
    let mut buf = Vec::new();
    if let Some(pipe) = pipe {
        let _ = pipe.take(MAX_OUTPUT).read_to_end(&mut buf).await;
    }
    buf
}

/// Result of a direct start of the notifier app.
enum Direct {
    /// The user allowed the request and the running notice shows.
    Running(Running),
    /// Result of the app without the running notice.
    Reply(Reply),
}

/// Run the executable of the notifier app as a child process. When the
/// user allows a request that has a running notice, the app keeps the
/// notice until the [`Running`] value drops.
///
/// Returns:
///   Result of the app, or error text when the app does not start or gives
///   no answer.
async fn run_direct(
    app: &VerifiedPath,
    text: &PromptText,
    deadline: Instant,
) -> Result<Direct, String> {
    let exe = app.join(notifier::EXE).map_err(|e| e.to_string())?;
    let Some(input) = request(text, deadline, true)? else {
        return Ok(Direct::Reply(timeout()));
    };
    let mut child = Command::new(exe.to_path().map_err(|e| e.to_string())?)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // The app stops at the end of its time limit, or when airlock
        // closes its input. A kill is the last resort: then the
        // notification stays, but a late click has no effect.
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    // Keep the input open while the app runs. When airlock stops, the
    // input closes and the app stops too.
    let mut pipe = child.stdin.take().ok_or("no child input")?;
    // The app can stop before it reads the request, for example on a
    // startup error. Then its answer tells more than the write error.
    let _ = pipe.write_all(&input).await;
    let mut stdin = Some(pipe);

    let stdout = child.stdout.take().ok_or("no child output")?;
    let mut stdout = BufReader::new(stdout.take(MAX_OUTPUT));
    // The diagnostics matter only for errors. Read them on the side, so
    // that a running notice does not hold the answer.
    let stderr = tokio::spawn(read_capped(child.stderr.take()));
    let mut answer = Vec::new();
    {
        let first = stdout.read_until(b'\n', &mut answer);
        tokio::pin!(first);
        if tokio::time::timeout_at(deadline, &mut first).await.is_err() {
            // Close the input: the app removes the notification, answers
            // and stops.
            stdin = None;
            if tokio::time::timeout(APP_GRACE, &mut first).await.is_err() {
                return Ok(Direct::Reply(timeout()));
            }
        }
    }

    let allowed = protocol::parse_answer(&answer).is_ok_and(|a| a.decision == ChildDecision::Allow);
    if allowed
        && text.running.is_some()
        && let Some(stdin) = stdin.take()
    {
        // Logs the alert style hint.
        read_reply(&answer, "")?;
        let (running, control) = Running::new();
        tokio::spawn(watch_running(child, stdin, stdout, control));
        return Ok(Direct::Running(running));
    }
    // Without the notice the app stops after the answer.
    drop(stdin);
    let stderr = tokio::time::timeout(APP_GRACE, stderr)
        .await
        .ok()
        .and_then(Result::ok)
        .unwrap_or_default();
    let _ = tokio::time::timeout(APP_GRACE, child.wait()).await;
    read_reply(&answer, &String::from_utf8_lossy(&stderr)).map(Direct::Reply)
}

/// Keep the running notice of the notifier app until the notice must go
/// away or the user aborts. Then close the input of the app, so that it
/// removes the notice and stops.
/// Args:
///  - `child`: Notifier app process
///  - `stdin`: Input of the app
///  - `stdout`: Output of the app after the answer line
///  - `control`: Platform side of the running notice
async fn watch_running(
    mut child: Child,
    stdin: ChildStdin,
    mut stdout: impl AsyncBufRead + Unpin,
    control: RunningControl,
) {
    let RunningControl { abort, mut close } = control;
    let mut line = Vec::new();
    loop {
        line.clear();
        tokio::select! {
            _ = &mut close => break,
            read = stdout.read_until(b'\n', &mut line) => {
                // End of output: the app stopped and the notice is gone.
                if !matches!(read, Ok(n) if n > 0) {
                    break;
                }
                if protocol::parse_event(&line) == Some(ChildEvent::Abort) {
                    let _ = abort.send(());
                    break;
                }
            }
        }
    }
    drop(stdin);
    // The kill on drop stops the app if it does not stop in time.
    let _ = tokio::time::timeout(APP_GRACE, child.wait()).await;
}

/// Start the notifier app through the system app launcher. Then the system
/// can ask the user for the notification permission. The launcher connects
/// the input and the output of the app to files in a private temporary
/// directory.
///
/// Returns:
/// Args:
///  - `dir`: Support directory for the temporary directory
///
/// Returns:
///   Result of the app, or error text when the app does not start or gives
///   no answer.
async fn run_open(
    app: &VerifiedPath,
    text: &PromptText,
    deadline: Instant,
    dir: &VerifiedPath,
) -> Result<Reply, String> {
    let run = TempDir(notifier::private_temp_dir(dir).map_err(|e| format!("{e:#}"))?);
    let file = |name: &str| run.0.join(name).map_err(|e| e.to_string());
    let (input, output, errors) = (file("request")?, file("answer")?, file("stderr")?);
    // The app gets its input from a file here, so airlock cannot close
    // the running notice. Thus do not ask for it.
    let Some(request) = request(text, deadline, false)? else {
        return Ok(timeout());
    };
    input.write(request).map_err(|e| e.to_string())?;
    output.write("").map_err(|e| e.to_string())?;
    errors.write("").map_err(|e| e.to_string())?;
    let path = |p: &VerifiedPath| {
        p.to_path()
            .map(|p| p.as_os_str().to_owned())
            .map_err(|e| e.to_string())
    };
    // -n: new instance, -g: do not bring the app to the front, -W: wait
    // until the app stops.
    // The launcher stops when the app stops. The app is not a child of
    // airlock and its input is a file, so only its own time limit stops
    // it. That limit ends before the deadline of airlock, so the app
    // removes the notification and answers in time. When airlock stops
    // first, the notification stays until that limit, and a click on it
    // has no effect.
    let child = Command::new(OPEN)
        .args(["-n", "-g", "-W", "--stdin"])
        .arg(path(&input)?)
        .arg("--stdout")
        .arg(path(&output)?)
        .arg("--stderr")
        .arg(path(&errors)?)
        .arg(path(app)?)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| e.to_string())?;
    let out = match tokio::time::timeout_at(deadline, child.wait_with_output()).await {
        Err(_) => return Ok(timeout()),
        Ok(out) => out.map_err(|e| e.to_string())?,
    };
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(msg::notifier_open_failed(out.status, err.trim()));
    }
    let answer = output
        .read_to_string_capped(MAX_OUTPUT)
        .map_err(|e| e.to_string())?;
    let diagnostics = errors.read_to_string_capped(MAX_OUTPUT).unwrap_or_default();
    read_reply(answer.as_bytes(), &diagnostics)
}

/// Read the result from the output of the notifier app. Logs the alert
/// style hint once per process when the notification goes away by itself.
/// Args:
///  - `stdout`: Answer output of the app
///  - `stderr`: Diagnostics of the app
///
/// Returns:
///   Result of the app, or error text with the diagnostics when the output
///   is not an answer.
fn read_reply(stdout: &[u8], stderr: &str) -> Result<Reply, String> {
    static HINTED: AtomicBool = AtomicBool::new(false);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        tracing::debug!("grants: notifier app: {stderr}");
    }
    let answer = protocol::parse_answer(stdout).map_err(|e| format!("{e}: {stderr}"))?;
    if answer.needs_alert_hint() && !HINTED.swap(true, Ordering::Relaxed) {
        tracing::info!("{}", msg::ALERT_STYLE_HINT);
    }
    Ok(answer.reply())
}

/// Temporary directory that is removed when the value drops.
struct TempDir(VerifiedPath);

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = self.0.remove_dir_all();
    }
}

/// Show the request as a dialog and wait for the answer.
/// Args:
///  - `text`: Clean prompt text
///  - `timeout`: Maximum wait time
///
/// Returns:
///   Decision of the user, deny on timeout. Error with the deny reason
///   when the dialog cannot show.
async fn dialog(text: &PromptText, timeout: Duration) -> Result<Decision, String> {
    let secs = timeout.as_secs().max(1).to_string();
    // The runner reads options only up to the first argument that does
    // not start with `-`. The title always starts with `airlock:`, so the
    // runner never reads request text as an option.
    let child = tokio::process::Command::new(OSASCRIPT)
        .arg("-e")
        .arg(SCRIPT)
        .arg(&text.title)
        .arg(text.details_text())
        .arg(msg::ALLOW)
        .arg(msg::DENY)
        .arg(&secs)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        // Late answers must have no effect: the dialog closes with the
        // wait.
        .kill_on_drop(true)
        .spawn();
    let child = child.map_err(msg::unavailable)?;
    let out = match tokio::time::timeout(timeout + GRACE, child.wait_with_output()).await {
        Err(_) => return Ok(Decision::Deny(msg::TIMEOUT.into())),
        // The dialog can show before the wait fails, so this is a
        // decision.
        Ok(Err(e)) => return Ok(Decision::Deny(msg::unavailable(e))),
        Ok(Ok(out)) => out,
    };
    // The dialog has no cancel button, so an error exit means that the
    // dialog did not show.
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        return Err(msg::unavailable(err.trim()));
    }
    Ok(match String::from_utf8_lossy(&out.stdout).trim() {
        s if s == msg::ALLOW => Decision::Allow,
        s if s == msg::DENY => Decision::DeniedByUser,
        _ => Decision::Deny(msg::TIMEOUT.into()),
    })
}
