//! User approval of temporary network access and network rule changes.
//!
//! Asks the user on the host to allow or deny a request from the sandbox:
//! run one command with network access to more hosts or with an on-demand
//! rule, or add or change a network rule. The request shows as a short
//! desktop notification with allow and deny buttons. On macOS, a small
//! notifier app shows the notification, and it can also show all details
//! of the request in a window. A system dialog asks when the notification
//! cannot show. The text from the sandbox is cleaned and cut
//! before it shows. No answer in time, or no notification support, means
//! deny. A late answer has no effect.

#[cfg(not(target_os = "macos"))]
pub(crate) mod dialog;
#[cfg(not(target_os = "macos"))]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
// The tests check the notifier app protocol and install on all hosts.
#[cfg(all(test, not(target_os = "macos")))]
mod macos {
    mod notifier;
    mod protocol;
}
pub(crate) mod text;

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::time::Duration;

use text::PromptText;
use tokio::sync::oneshot;

use crate::config::config_values::NetworkRule;
use crate::data_dir::DataDir;

/// Request that the user must approve.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrantPrompt {
    /// Name of the sandbox (project) that asks.
    pub sandbox: String,
    /// What the sandbox asks for.
    pub request: PromptRequest,
}

/// What the sandbox asks for. All text is untrusted.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PromptRequest {
    /// Run one command with access to these host patterns. A single `*`
    /// asks for all public hosts.
    Hosts {
        /// Host patterns.
        hosts: Vec<String>,
        /// Command to run.
        command: PromptCommand,
    },
    /// Run one command with this on-demand rule.
    Rule {
        /// Name of the rule.
        name: String,
        /// The rule as it applies now.
        rule: NetworkRule,
        /// Command to run.
        command: PromptCommand,
    },
    /// Add or change a network rule.
    RuleChange {
        /// Name of the rule.
        name: String,
        /// The rule before the change, as it applies now. `None` for a new
        /// rule.
        old: Option<NetworkRule>,
        /// The rule after the change, as it applies after the change.
        rule: NetworkRule,
        /// Other keys of the rule table in the project file, as `(key,
        /// value)` text. The rule does not use them. The change keeps them.
        other: Vec<(String, String)>,
    },
}

/// Command of an access request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PromptCommand {
    /// Shell command.
    pub cmd: String,
    /// Working directory in the sandbox.
    pub cwd: String,
    /// More environment of the command, as `KEY=VALUE` entries.
    pub env: Vec<String>,
    /// Maximum duration of the access, in seconds.
    pub duration_secs: u32,
}

/// Decision of the user.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Decision {
    /// The user allowed the request.
    Allow,
    /// The user denied the request.
    DeniedByUser,
    /// The request is denied for another reason. The text tells why (timeout,
    /// no notification support).
    Deny(String),
}

/// Decision of the user, and the notice that shows while the approved
/// command runs.
#[derive(Debug)]
pub struct Approval {
    /// Decision of the user.
    pub decision: Decision,
    /// Notice with an abort button, if the host can show one. Only an
    /// allowed command request has it.
    pub running: Option<Running>,
}

impl From<Decision> for Approval {
    fn from(decision: Decision) -> Self {
        Self {
            decision,
            running: None,
        }
    }
}

/// Notice that shows while an approved command runs. The user can abort
/// the command from it. The notice goes away when this value drops.
#[derive(Debug)]
pub struct Running {
    abort: Option<oneshot::Receiver<()>>,
    _close: oneshot::Sender<()>,
}

/// Platform side of a [`Running`] notice.
pub(crate) struct RunningControl {
    /// Send when the user selects abort.
    pub abort: oneshot::Sender<()>,
    /// Closes when the notice must go away.
    pub close: oneshot::Receiver<()>,
}

impl Running {
    /// Make a notice and its platform side.
    pub(crate) fn new() -> (Self, RunningControl) {
        let (abort_tx, abort_rx) = oneshot::channel();
        let (close_tx, close_rx) = oneshot::channel();
        let running = Self {
            abort: Some(abort_rx),
            _close: close_tx,
        };
        let control = RunningControl {
            abort: abort_tx,
            close: close_rx,
        };
        (running, control)
    }

    /// Take the abort signal. It resolves with `Ok` when the user selects
    /// abort, and with `Err` when the notice goes away with no abort.
    pub fn take_abort(&mut self) -> Option<oneshot::Receiver<()>> {
        self.abort.take()
    }
}

/// Asks the user for a decision.
pub trait Approver {
    /// Show the request to the user and wait for the decision.
    /// Args:
    ///  - `prompt`: Request to show
    ///  - `timeout`: Maximum wait time. When it ends, the decision is deny.
    fn approve(
        &self,
        prompt: GrantPrompt,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Approval> + 'static>>;
}

/// Get the approver for this host: a desktop notification. On macOS, a
/// system dialog when the notification cannot show.
/// Args:
///  - `data`: Data directory. On macOS, the notifier app installs there.
pub(crate) fn system(data: &DataDir) -> Rc<dyn Approver> {
    #[cfg(target_os = "macos")]
    {
        Rc::new(SystemApprover { data: data.clone() })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = data;
        Rc::new(SystemApprover {})
    }
}

/// Approver that asks the user on the host desktop.
struct SystemApprover {
    /// Data directory for the notifier app.
    #[cfg(target_os = "macos")]
    data: DataDir,
}

impl Approver for SystemApprover {
    fn approve(
        &self,
        prompt: GrantPrompt,
        timeout: Duration,
    ) -> Pin<Box<dyn Future<Output = Approval> + 'static>> {
        #[cfg(target_os = "macos")]
        {
            let data = self.data.clone();
            Box::pin(async move {
                let text = log_pending(&prompt, timeout);
                let sandbox = text.sandbox.clone();
                log_decision(&sandbox, macos::ask(text, timeout, &data).await)
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            Box::pin(async move {
                let text = log_pending(&prompt, timeout);
                let sandbox = text.sandbox.clone();
                log_decision(&sandbox, linux::ask(text, timeout).await)
            })
        }
    }
}

/// Make the clean text of the request and log that it waits for the user.
fn log_pending(prompt: &GrantPrompt, timeout: Duration) -> PromptText {
    let text = PromptText::new(prompt);
    // Printing to the terminal would break the screen of the agent that
    // runs in it. Thus the line goes only to the log.
    tracing::info!(
        "{}",
        crate::messages::grants::pending(&text.sandbox, &text.title, timeout.as_secs())
    );
    text
}

/// Log the decision of the user and return the approval.
fn log_decision(sandbox: &str, approval: Approval) -> Approval {
    match &approval.decision {
        Decision::Allow => tracing::info!("grants: {sandbox}: user allowed the request"),
        Decision::DeniedByUser => tracing::info!("grants: {sandbox}: user denied the request"),
        Decision::Deny(why) => tracing::info!("grants: {sandbox}: request denied: {why}"),
    }
    approval
}
