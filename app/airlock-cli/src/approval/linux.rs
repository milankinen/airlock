//! Approval through a desktop notification with allow and deny buttons.
//!
//! The notification also has a details button. It opens a window with all
//! details of the request, when the desktop has an X display. If the window
//! cannot show, the notification shows again with no details button. The
//! same window asks the user when the notification cannot show. With no X
//! display, the request is denied when the desktop has no notification
//! service, or when the service cannot show buttons. After the user allows
//! a command, a notice with an abort button stays until the command ends.

use std::sync::Arc;
use std::time::Duration;

use notify_rust::{Hint, Notification, NotificationResponse, Timeout, Urgency};

use super::text::{PromptText, escape_markup};
use super::{Approval, Decision, Running, RunningControl, dialog};
use crate::messages::grants as msg;

/// Action ID of the allow button.
const ACTION_ALLOW: &str = "allow";
/// Action ID of the deny button.
const ACTION_DENY: &str = "deny";
/// Action ID of the details button.
const ACTION_DETAILS: &str = "details";
/// Action ID of the abort button.
const ACTION_ABORT: &str = "abort";

/// Maximum wait time for the capabilities of the notification service.
/// With no service, the request is denied quickly.
const CAPS_TIMEOUT: Duration = Duration::from_secs(5);

/// One permit for the thread that gets the capabilities. A hung service
/// then holds at most one thread.
static CAPS_PERMIT: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// Get the capabilities of the notification service. Waits for an earlier
/// request in the same time limit.
/// Args:
///  - `limit`: Maximum wait time
///
/// Returns:
///   Capability names, or an error text.
async fn capabilities(limit: Duration) -> Result<Vec<String>, String> {
    let get = async {
        let permit = CAPS_PERMIT.acquire().await.map_err(|e| e.to_string())?;
        // The library gets the capabilities with a blocking call. It must
        // not block the thread of the async runtime. The thread keeps the
        // permit until it ends, also when it panics or when the caller
        // stops the wait.
        let task = tokio::task::spawn_blocking(move || {
            let _permit = permit;
            notify_rust::get_capabilities()
        });
        match task.await {
            Err(e) => Err(e.to_string()),
            Ok(caps) => caps.map_err(|e| e.to_string()),
        }
    };
    tokio::time::timeout(limit, get)
        .await
        .unwrap_or_else(|_| Err(msg::NOTIFY_NO_ANSWER.into()))
}

/// Show the request as a notification and wait for the answer.
/// Args:
///  - `text`: Clean prompt text
///  - `timeout`: Maximum wait time
///
/// Returns:
///   Decision of the user. Deny on timeout or when the host cannot show
///   the notification.
pub(super) async fn ask(text: PromptText, timeout: Duration) -> Approval {
    let running = text.running.clone();
    let markup = std::cell::Cell::new(false);
    let decision = ask_decision(&text, timeout, &markup).await;
    let running = match (&decision, running) {
        (Decision::Allow, Some(body)) => {
            let body = if markup.get() {
                escape_markup(&body)
            } else {
                body
            };
            let (running, control) = Running::new();
            tokio::spawn(show_running(body, control));
            Some(running)
        }
        _ => None,
    };
    Approval { decision, running }
}

/// Show the notice with the abort button until the command ends or the
/// user selects abort.
/// Args:
///  - `body`: Body text of the notice, ready to show
///  - `control`: Platform side of the notice
async fn show_running(body: String, control: RunningControl) {
    let RunningControl { abort, mut close } = control;
    let notification = Notification::new()
        .appname("airlock")
        .summary(msg::RUNNING_TITLE)
        .body(&body)
        .action(ACTION_ABORT, msg::ABORT)
        .urgency(Urgency::Normal)
        .hint(Hint::Resident(true))
        .hint(Hint::SuppressSound(true))
        .timeout(Timeout::Never)
        .finalize();
    let handle = tokio::select! {
        shown = tokio::time::timeout(CLOSE_TIMEOUT, notification.show_async()) => match shown {
            Ok(Ok(handle)) => Arc::new(handle),
            Ok(Err(e)) => {
                tracing::debug!("grants: running notice failed: {e}");
                return;
            }
            Err(_) => return,
        },
        _ = &mut close => return,
    };
    // The library panics on some D-Bus errors while it waits. A separate
    // task keeps such a panic away from this task.
    let waiter = {
        let handle = handle.clone();
        tokio::spawn(async move {
            let mut answer = None;
            handle
                .wait_for_action_async(|r| answer = Some(r.clone()))
                .await;
            matches!(answer, Some(NotificationResponse::Action(a)) if a == ACTION_ABORT)
        })
    };
    let stop = waiter.abort_handle();
    tokio::select! {
        clicked = waiter => {
            if clicked.unwrap_or(false) {
                let _ = abort.send(());
            }
        }
        _ = &mut close => stop.abort(),
    }
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, handle.close_async()).await;
}

/// Show the request and wait for the decision. Sets `markup` when the
/// service shows body markup. Uses a dialog when the notification cannot
/// show and the host has an X display.
async fn ask_decision(
    text: &PromptText,
    timeout: Duration,
    markup_out: &std::cell::Cell<bool>,
) -> Decision {
    let deadline = tokio::time::Instant::now() + timeout;
    let mut dialog = dialog::available();
    // Each pass shows the notification and then, if necessary, the dialog.
    // A second pass occurs only when the dialog fails after the details
    // button, and it has no details button. Thus the loop ends.
    loop {
        let failed = match notify(text, deadline, dialog, markup_out).await {
            Ok(Answer::Decision(decision)) => return decision,
            Ok(Answer::Details) => None,
            Err(why) if dialog => {
                tracing::warn!("grants: notification failed, using a dialog: {why}");
                Some(why)
            }
            Err(why) => return Decision::Deny(why),
        };
        match dialog::ask(text, time_left(deadline)).await {
            Ok(decision) => return decision,
            // The notification failed too. Nothing can show the request.
            Err(e) if failed.is_some() => return Decision::Deny(msg::unavailable(e)),
            Err(e) => {
                // The user asked for the details. Do not deny for them:
                // show the notification again, with no details button.
                tracing::warn!("grants: details dialog failed: {e}");
                dialog = false;
            }
        }
    }
}

/// Get the time to the deadline. Zero after the deadline, and a dialog then
/// gives a timeout answer at once.
fn time_left(deadline: tokio::time::Instant) -> Duration {
    deadline.saturating_duration_since(tokio::time::Instant::now())
}

/// Answer of the user to the notification.
enum Answer {
    /// The user decided with a button, or closed the notification.
    Decision(Decision),
    /// The user asks for the details window.
    Details,
}

/// Show the request as a notification and wait for the answer. The
/// notification is removed before the return.
/// Args:
///  - `text`: Clean prompt text
///  - `deadline`: End of the wait
///  - `dialog`: True if the notification has the details button
///  - `markup_out`: Set when the service shows body markup
///
/// Returns:
///   Answer of the user. Timeout is a decision (deny), not an error.
///   Error text when the notification cannot show.
async fn notify(
    text: &PromptText,
    deadline: tokio::time::Instant,
    dialog: bool,
    markup_out: &std::cell::Cell<bool>,
) -> Result<Answer, String> {
    let caps = match capabilities(CAPS_TIMEOUT.min(time_left(deadline))).await {
        Ok(caps) => caps,
        Err(e) => return Err(msg::unavailable(e)),
    };
    if !caps.iter().any(|c| c == "actions") {
        return Err(msg::NO_ACTIONS.into());
    }
    let markup = caps.iter().any(|c| c == "body-markup");
    markup_out.set(markup);
    // The body can point to the details button. Do not show that text when
    // the notification has no such button.
    let body = if text.details_hint && !dialog {
        msg::NO_DETAILS
    } else {
        &text.body
    };
    let body = if markup {
        escape_markup(body)
    } else {
        body.to_string()
    };

    let mut notification = Notification::new();
    notification
        .appname("airlock")
        .summary(&text.title)
        .body(&body)
        .action(ACTION_ALLOW, msg::ALLOW)
        .action(ACTION_DENY, msg::DENY);
    if dialog {
        // No "default" action for a click on the body. Some servers show it
        // as one more button, and many servers send no click on the body.
        notification.action(ACTION_DETAILS, msg::MORE_DETAILS);
    }
    let notification = notification
        .urgency(Urgency::Critical)
        .hint(Hint::Resident(true))
        .timeout(Timeout::Never)
        .finalize();
    let handle = match tokio::time::timeout_at(deadline, notification.show_async()).await {
        Err(_) => return Ok(Answer::Decision(Decision::Deny(msg::TIMEOUT.into()))),
        Ok(Err(e)) => return Err(msg::unavailable(e)),
        Ok(Ok(handle)) => Arc::new(handle),
    };

    // The library panics on some D-Bus errors while it waits. A separate
    // task keeps such a panic away from the caller.
    let waiter = {
        let handle = handle.clone();
        tokio::spawn(async move { wait_for_answer(&handle, dialog).await })
    };
    let abort = waiter.abort_handle();
    let answer = match tokio::time::timeout_at(deadline, waiter).await {
        Ok(Ok(answer)) => answer,
        Ok(Err(e)) => Answer::Decision(Decision::Deny(msg::unavailable(e))),
        Err(_) => {
            // Stop the wait first, so that a late answer has no effect.
            abort.abort();
            Answer::Decision(Decision::Deny(msg::TIMEOUT.into()))
        }
    };
    // A resident notification stays after a button click. Remove it in
    // all cases, also before the details window shows: its buttons have no
    // effect after the wait. The close call has its own time limit, because
    // a hung bus must not delay the decision.
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, handle.close_async()).await;
    Ok(answer)
}

/// Maximum wait time for the removal of the notification.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Wait until the user selects a button or closes the notification.
/// Any other answer is a deny.
/// Args:
///  - `handle`: The shown notification
///  - `details`: True if the notification has the details button
async fn wait_for_answer(handle: &notify_rust::NotificationHandle, details: bool) -> Answer {
    let mut answer = None;
    handle
        .wait_for_action_async(|r| answer = Some(r.clone()))
        .await;
    match answer {
        Some(NotificationResponse::Action(a)) if a == ACTION_ALLOW => {
            Answer::Decision(Decision::Allow)
        }
        Some(NotificationResponse::Action(a)) if a == ACTION_DENY => {
            Answer::Decision(Decision::DeniedByUser)
        }
        Some(NotificationResponse::Action(a)) if details && a == ACTION_DETAILS => Answer::Details,
        _ => Answer::Decision(Decision::Deny(msg::DISMISSED.into())),
    }
}
