//! TUI monitor runtime.
//!
//! Shows the guest in the TUI monitor control panel, together with network
//! events and guest stats.

use airlock_common::supervisor_capnp::stdin;
use futures::StreamExt;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

use super::{OutputSink, PtySize, Runtime, SignalStream, Terminal};
use crate::network::NetworkHandle;
use crate::project::Project;
use crate::rpc;

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
        }
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

    fn launch(
        self,
        project: &Project,
        network: &NetworkHandle,
        supervisor: rpc::Supervisor,
    ) -> anyhow::Result<MonitorTerminal> {
        let stdin_tx = self
            .stdin_tx
            .ok_or_else(|| anyhow::anyhow!("attach_stdin must be called before launch"))?;
        let sig_tx = self
            .sig_tx
            .ok_or_else(|| anyhow::anyhow!("signals must be called before launch"))?;
        let project_path = project.host_cwd.display().to_string();
        let control: std::sync::Arc<dyn airlock_monitor::NetworkControl> =
            std::sync::Arc::new(network.control());
        let tui = airlock_monitor::spawn(
            stdin_tx,
            sig_tx,
            control,
            project_path,
            crate::cli::version_string(false),
            self.settings,
        );

        let mut tasks = JoinSet::new();

        // Forward network events from the broadcast channel to the TUI
        // thread.
        let net_tx = tui.tx.clone();
        let mut events = network.events();
        tasks.spawn_local(async move {
            loop {
                match events.recv().await {
                    Ok(ev) => net_tx.send_network(ev),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Poll the guest CPU and memory stats one time per second and send
        // them to the TUI.
        let stats_tx = tui.tx.clone();
        tasks.spawn_local(async move {
            let mut ticker = tokio::time::interval(std::time::Duration::from_secs(1));
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                match supervisor.poll_stats().await {
                    Ok(snap) => stats_tx.send_stats(airlock_monitor::StatsSnapshot {
                        per_core: snap.per_core,
                        total_bytes: snap.total_bytes,
                        used_bytes: snap.used_bytes,
                        load_avg: snap.load_avg,
                    }),
                    Err(e) => {
                        tracing::debug!("poll_stats failed: {e}");
                        break;
                    }
                }
            }
        });

        Ok(MonitorTerminal {
            tui: Some(tui),
            _tasks: tasks,
        })
    }
}

/// Output sink that sends guest output to the TUI monitor.
pub struct MonitorTerminal {
    tui: Option<airlock_monitor::TuiHandle>,
    /// Forwarders of network events and stats. They stop when the terminal
    /// drops (after `exit`).
    _tasks: JoinSet<()>,
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
        let Some(tui) = self.tui.take() else {
            return exit_code;
        };
        tui.tx.send_exit(exit_code);
        tui.join().unwrap_or(exit_code)
    }
}
