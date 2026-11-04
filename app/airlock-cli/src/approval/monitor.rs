//! Approval in the monitor TUI.
//!
//! When the host cannot show a notification or a system dialog, and the
//! monitor TUI is open, the request shows as a dialog in the TUI. The TUI
//! registers itself while it runs. The TUI can run in this process, or in
//! a client of the sandbox process that attached a monitor. When more TUIs
//! run, the newest one asks. When it stops, the next newest one asks. The
//! dialog goes away when the time limit ends, so that a late answer has no
//! effect.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_monitor::{
    ApprovalDetail, ApprovalDiffKind, ApprovalDiffLine, ApprovalRequest, TuiSender,
};
use tokio::sync::{Notify, oneshot};
use tokio::time::Instant;

use super::Decision;
use super::text::{DetailItem, DiffKind, PromptText};
use crate::messages::grants as msg;

/// Dialog that shows approval requests to the user.
pub(crate) trait MonitorDialog {
    /// Show the request.
    /// Args:
    ///  - `request`: The request. The dialog sends the answer to its reply
    ///    channel.
    ///  - `deadline`: End of the time limit. After it, the caller drops the
    ///    receiver of the reply channel.
    ///
    /// Returns:
    ///   The request back if the dialog cannot show it.
    fn show(&self, request: ApprovalRequest, deadline: Instant) -> Result<(), ApprovalRequest>;

    /// Tell if the dialog can still show requests. A dialog that is not
    /// live does not get new requests.
    fn is_live(&self) -> bool {
        true
    }
}

impl MonitorDialog for TuiSender {
    fn show(&self, request: ApprovalRequest, _deadline: Instant) -> Result<(), ApprovalRequest> {
        // The TUI removes the request when the caller drops the receiver.
        self.send_approval(request)
    }
}

/// Shared place for the dialogs of the monitor TUIs. It is empty when no
/// TUI runs. More TUIs can register. The newest live dialog asks. All
/// clones share the same place.
#[derive(Clone, Default)]
pub(crate) struct MonitorSlot(Rc<Dialogs>);

/// Registered dialogs, the oldest first, and the signal for a new one.
#[derive(Default)]
struct Dialogs {
    list: RefCell<Vec<Rc<dyn MonitorDialog>>>,
    added: Notify,
}

impl MonitorSlot {
    /// Register the dialog of a TUI that starts. It asks until it is
    /// removed or a newer dialog registers.
    pub(crate) fn set(&self, dialog: Rc<dyn MonitorDialog>) {
        self.0.list.borrow_mut().push(dialog);
        self.0.added.notify_waiters();
    }

    /// Remove all dialogs when the TUI stops.
    pub(crate) fn clear(&self) {
        self.0.list.borrow_mut().clear();
    }

    /// Remove `dialog` only. The other dialogs stay.
    pub(crate) fn clear_if(&self, dialog: &Rc<dyn MonitorDialog>) {
        self.0.list.borrow_mut().retain(|d| !Rc::ptr_eq(d, dialog));
    }

    /// Get the newest live dialog, if a TUI runs. Dialogs that are not
    /// live are removed.
    pub(crate) fn get(&self) -> Option<Rc<dyn MonitorDialog>> {
        let mut list = self.0.list.borrow_mut();
        list.retain(|d| d.is_live());
        list.last().cloned()
    }

    /// Get the newest live dialog. If there is none, wait for a dialog to
    /// register.
    /// Args:
    ///  - `deadline`: End of the wait
    ///
    /// Returns:
    ///   The dialog, or `None` if no dialog registered before the deadline.
    pub(crate) async fn wait(&self, deadline: Instant) -> Option<Rc<dyn MonitorDialog>> {
        loop {
            // Make the waiter before the check. A `set` after the check
            // then wakes it, also before the first poll.
            let added = self.0.added.notified();
            if let Some(dialog) = self.get() {
                return Some(dialog);
            }
            tokio::time::timeout_at(deadline, added).await.ok()?;
        }
    }

    /// Number of registered dialogs, also the dialogs that are not live.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.0.list.borrow().len()
    }
}

/// Show the request in the monitor TUI and wait for the answer.
/// Args:
///  - `dialog`: Dialog of the TUI
///  - `text`: Clean prompt text
///  - `deadline`: End of the time limit
///
/// Returns:
///   Decision of the user. Deny on timeout or when the TUI stopped.
pub(super) async fn ask(
    dialog: &dyn MonitorDialog,
    text: &PromptText,
    deadline: Instant,
) -> Decision {
    let (reply, answer) = oneshot::channel();
    let request = ApprovalRequest {
        title: text.title.clone(),
        body: text.body.clone(),
        details: text.details.iter().map(detail).collect(),
        reply,
    };
    if dialog.show(request, deadline).is_err() {
        return Decision::Deny(msg::MONITOR_STOPPED.into());
    }
    // On timeout the receiver drops here. Then the TUI removes the dialog.
    match tokio::time::timeout_at(deadline, answer).await {
        Err(_) => Decision::Deny(msg::TIMEOUT.into()),
        Ok(Err(_)) => Decision::Deny(msg::MONITOR_STOPPED.into()),
        Ok(Ok(true)) => Decision::Allow,
        Ok(Ok(false)) => Decision::DeniedByUser,
    }
}

/// Convert one detail of the prompt text to the TUI form.
fn detail(item: &DetailItem) -> ApprovalDetail {
    ApprovalDetail {
        label: item.label.clone(),
        value: item.value.clone(),
        mono: item.mono,
        diff: item.diff.as_ref().map(|lines| {
            lines
                .iter()
                .map(|l| ApprovalDiffLine {
                    kind: match l.kind {
                        DiffKind::Add => ApprovalDiffKind::Add,
                        DiffKind::Remove => ApprovalDiffKind::Remove,
                        DiffKind::Same => ApprovalDiffKind::Same,
                    },
                    text: l.text.clone(),
                })
                .collect()
        }),
    }
}
