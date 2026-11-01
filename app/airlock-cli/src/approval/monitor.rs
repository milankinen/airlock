//! Approval in the monitor TUI.
//!
//! When the host cannot show a notification or a system dialog, and the
//! monitor TUI is open, the request shows as a dialog in the TUI. The TUI
//! registers itself while it runs. The dialog goes away when the time
//! limit ends, so that a late answer has no effect.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_monitor::{
    ApprovalDetail, ApprovalDiffKind, ApprovalDiffLine, ApprovalRequest, TuiSender,
};
use tokio::sync::oneshot;
use tokio::time::Instant;

use super::Decision;
use super::text::{DetailItem, DiffKind, PromptText};
use crate::messages::grants as msg;

/// Dialog that shows approval requests to the user.
pub(crate) trait MonitorDialog {
    /// Show the request.
    ///
    /// Returns:
    ///   The request back if the dialog cannot show it.
    fn show(&self, request: ApprovalRequest) -> Result<(), ApprovalRequest>;
}

impl MonitorDialog for TuiSender {
    fn show(&self, request: ApprovalRequest) -> Result<(), ApprovalRequest> {
        self.send_approval(request)
    }
}

/// Shared place for the dialog of the monitor TUI. It is empty when no TUI
/// runs. All clones share the same place.
#[derive(Clone, Default)]
pub(crate) struct MonitorSlot(Rc<RefCell<Option<Rc<dyn MonitorDialog>>>>);

impl MonitorSlot {
    /// Register the dialog of a TUI that starts.
    pub(crate) fn set(&self, dialog: Rc<dyn MonitorDialog>) {
        *self.0.borrow_mut() = Some(dialog);
    }

    /// Remove the dialog when the TUI stops.
    pub(crate) fn clear(&self) {
        self.0.borrow_mut().take();
    }

    /// Get the registered dialog, if a TUI runs.
    pub(super) fn get(&self) -> Option<Rc<dyn MonitorDialog>> {
        self.0.borrow().clone()
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
    if dialog.show(request).is_err() {
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
