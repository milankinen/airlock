//! TUI monitor runtime.
//!
//! Shows the guest in the TUI monitor control panel, together with network
//! events and guest stats. While the TUI runs, it can ask the user to
//! approve requests from the sandbox.

use airlock_common::supervisor_capnp::stdin;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::{MonitorSource, OutputSink, PtySize, Runtime, SignalStream, Terminal};
use crate::approval::MonitorSlot;

/// Runtime with the TUI monitor. Call `attach_stdin` before `launch`, so
/// the supervisor gets its channel-backed stdin client.
pub struct MonitorRuntime {
    /// Sender of the channel-backed guest stdin. Set in `attach_stdin`.
    stdin_tx: Option<mpsc::Sender<airlock_monitor::TuiInputEvent>>,
    /// Sender for the TUI to request signals (for example SIGHUP and
    /// SIGTERM when the user stops the sandbox from the TUI). Taken in
    /// `launch`.
    sig_tx: Option<mpsc::Sender<i32>>,
    /// Receiver that `signals()` reads and merges into the signal stream.
    sig_rx: Option<mpsc::Receiver<i32>>,
    /// Buffer limits, scrollback and key bindings for the TUI. The CLI
    /// makes them from the `[monitor]` section of the user settings.
    settings: airlock_monitor::TuiSettings,
    /// Place of the approval dialog. It holds the dialog while the TUI runs.
    approvals: MonitorSlot,
}

impl MonitorRuntime {
    /// Make a monitor runtime with the given TUI settings.
    pub fn new(settings: airlock_monitor::TuiSettings) -> Self {
        let (sig_tx, sig_rx) = mpsc::channel(8);
        Self {
            stdin_tx: None,
            sig_tx: Some(sig_tx),
            sig_rx: Some(sig_rx),
            settings,
            approvals: MonitorSlot::default(),
        }
    }

    /// Get the place of the approval dialog of the TUI. It holds the dialog
    /// from `launch` until the terminal exits.
    pub fn approval_dialog(&self) -> MonitorSlot {
        self.approvals.clone()
    }
}

impl Runtime for MonitorRuntime {
    type Terminal = MonitorTerminal;

    fn attach_stdin(&mut self) -> anyhow::Result<(stdin::Client, PtySize)> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        // The tab bar uses rows at the bottom, so the guest PTY gets only
        // the body area. With the full terminal size, the guest draws past
        // the vt100 grid, and the extra lines overwrite the last row.
        let body_rows = rows.saturating_sub(airlock_monitor::TAB_BAR_HEIGHT);
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let tui_stdin = airlock_monitor::TuiStdin::new(rx, Some((body_rows, cols)));
        let pty_size = tui_stdin.pty_size();
        self.stdin_tx = Some(tx);
        Ok((capnp_rpc::new_client(tui_stdin), pty_size))
    }

    fn signals(&mut self) -> anyhow::Result<SignalStream> {
        let os = super::signals()?;
        let Some(mut tui_rx) = self.sig_rx.take() else {
            return Ok(os);
        };
        let merged = async_stream::stream! {
            let mut os = os;
            loop {
                tokio::select! {
                    Some(sig) = os.next() => yield sig,
                    Some(sig) = tui_rx.recv() => yield sig,
                    else => break,
                }
            }
        };
        Ok(Box::pin(merged))
    }

    fn launch(self, monitor: Option<MonitorSource>) -> anyhow::Result<MonitorTerminal> {
        let stdin_tx = self
            .stdin_tx
            .ok_or_else(|| anyhow::anyhow!(crate::messages::runtime::ATTACH_STDIN_BEFORE_LAUNCH))?;
        let sig_tx = self
            .sig_tx
            .ok_or_else(|| anyhow::anyhow!(crate::messages::runtime::SIGNALS_BEFORE_LAUNCH))?;
        let MonitorSource {
            project_path,
            control,
            mut events,
            mut stats,
            tasks: source_tasks,
        } = monitor
            .ok_or_else(|| anyhow::anyhow!(crate::messages::runtime::MONITOR_SOURCE_MISSING))?;
        let tui = airlock_monitor::spawn(
            stdin_tx,
            sig_tx,
            control,
            project_path,
            crate::cli::version_string(false),
            self.settings,
        );

        self.approvals.set(std::rc::Rc::new(tui.tx.clone()));
        let mut tasks = JoinSet::new();

        // Forward network events and guest stats from the source to the TUI
        // thread.
        let net_tx = tui.tx.clone();
        tasks.spawn_local(async move {
            while let Some(ev) = events.recv().await {
                net_tx.send_network(ev);
            }
        });
        let stats_tx = tui.tx.clone();
        tasks.spawn_local(async move {
            while let Some(snap) = stats.recv().await {
                stats_tx.send_stats(snap);
            }
        });

        Ok(MonitorTerminal {
            tui: Some(tui),
            _tasks: tasks,
            approvals: self.approvals,
            _source_tasks: source_tasks,
        })
    }
}

/// Output sink that sends guest output to the TUI monitor.
pub struct MonitorTerminal {
    tui: Option<airlock_monitor::TuiHandle>,
    /// Forwarders of network events and stats. They stop when the terminal
    /// drops (after `exit`).
    _tasks: JoinSet<()>,
    /// Tasks that feed the source. They stop with the terminal.
    _source_tasks: JoinSet<()>,
    /// Place of the approval dialog. Cleared when the TUI stops.
    approvals: MonitorSlot,
}

impl Drop for MonitorTerminal {
    fn drop(&mut self) {
        self.approvals.clear();
    }
}

impl OutputSink for MonitorTerminal {
    fn stdout(&mut self, bytes: &[u8]) {
        if let Some(tui) = &self.tui {
            tui.tx.send_output(bytes.to_vec());
        }
    }

    fn stderr(&mut self, bytes: &[u8]) {
        if let Some(tui) = &self.tui {
            tui.tx.send_output(bytes.to_vec());
        }
    }
}

impl Terminal for MonitorTerminal {
    fn exit(mut self, exit_code: i32) -> i32 {
        // New requests must not go to a TUI that stops.
        self.approvals.clear();
        let Some(tui) = self.tui.take() else {
            return exit_code;
        };
        tui.tx.send_exit(exit_code);
        tui.join().unwrap_or(exit_code)
    }
}
