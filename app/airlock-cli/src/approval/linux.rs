//! Approval through a desktop notification with allow and deny buttons.
//!
//! The request is denied when the desktop has no notification service, or
//! when the service cannot show buttons. After the user allows a command,
//! a notice with an abort button stays until the command ends.

use std::sync::Arc;
use std::time::Duration;

use notify_rust::{Hint, Notification, NotificationResponse, Timeout, Urgency};

use super::text::{PromptText, escape_markup};
use super::{Approval, Decision, Running, RunningControl};
use crate::messages::grants as msg;

/// Action ID of the allow button.
const ACTION_ALLOW: &str = "allow";
/// Action ID of the deny button.
const ACTION_DENY: &str = "deny";
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
    let decision = ask_decision(text, timeout, &markup).await;
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
/// service shows body markup.
async fn ask_decision(
    text: PromptText,
    timeout: Duration,
    markup_out: &std::cell::Cell<bool>,
) -> Decision {
    let deadline = tokio::time::Instant::now() + timeout;
    let caps = match capabilities(CAPS_TIMEOUT.min(timeout)).await {
        Ok(caps) => caps,
        Err(e) => return Decision::Deny(msg::unavailable(e)),
    };
    if !caps.iter().any(|c| c == "actions") {
        return Decision::Deny(msg::NO_ACTIONS.into());
    }
    let markup = caps.iter().any(|c| c == "body-markup");
    markup_out.set(markup);
    let body = if markup {
        escape_markup(&text.body)
    } else {
        text.body
    };

    let notification = Notification::new()
        .appname("airlock")
        .summary(&text.title)
        .body(&body)
        .action(ACTION_ALLOW, msg::ALLOW)
        .action(ACTION_DENY, msg::DENY)
        .urgency(Urgency::Critical)
        .hint(Hint::Resident(true))
        .timeout(Timeout::Never)
        .finalize();
    let handle = match tokio::time::timeout_at(deadline, notification.show_async()).await {
        Err(_) => return Decision::Deny(msg::TIMEOUT.into()),
        Ok(Err(e)) => return Decision::Deny(msg::unavailable(e)),
        Ok(Ok(handle)) => Arc::new(handle),
    };

    // The library panics on some D-Bus errors while it waits. A separate
    // task keeps such a panic away from the caller.
    let waiter = {
        let handle = handle.clone();
        tokio::spawn(async move { wait_for_decision(&handle).await })
    };
    let abort = waiter.abort_handle();
    let decision = match tokio::time::timeout_at(deadline, waiter).await {
        Ok(Ok(decision)) => decision,
        Ok(Err(e)) => Decision::Deny(msg::unavailable(e)),
        Err(_) => {
            // Stop the wait first, so that a late answer has no effect.
            abort.abort();
            Decision::Deny(msg::TIMEOUT.into())
        }
    };
    // A resident notification stays after a button click. Remove it in
    // all cases. The close call has its own time limit, because a hung
    // bus must not delay the decision.
    let _ = tokio::time::timeout(CLOSE_TIMEOUT, handle.close_async()).await;
    decision
}

/// Maximum wait time for the removal of the notification.
const CLOSE_TIMEOUT: Duration = Duration::from_secs(2);

/// Wait until the user selects a button or closes the notification.
/// Any other answer, for example a click on the notification body, is a
/// deny.
async fn wait_for_decision(handle: &notify_rust::NotificationHandle) -> Decision {
    let mut answer = None;
    handle
        .wait_for_action_async(|r| answer = Some(r.clone()))
        .await;
    match answer {
        Some(NotificationResponse::Action(a)) if a == ACTION_ALLOW => Decision::Allow,
        Some(NotificationResponse::Action(a)) if a == ACTION_DENY => Decision::DeniedByUser,
        _ => Decision::Deny(msg::DISMISSED.into()),
    }
}
