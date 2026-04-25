//! TUI monitor `Runtime`: channel-backed stdin, ratatui thread, and the
//! network-event / stats-poll forwarders.

use airlock_common::supervisor_capnp::stdin;
use futures::StreamExt;
use tokio::sync::mpsc;

use super::{PtySize, Runtime, SignalStream, Terminal};
use crate::network::Network;
use crate::project::Project;
use crate::rpc;

/// Build a TUI-backed runtime. `attach_stdin` must be called before `launch`
/// so the supervisor gets its channel-backed stdin client.
pub struct MonitorRuntime {
    stdin_tx: Option<mpsc::Sender<airlock_monitor::TuiInputEvent>>,
    /// Sender given to the TUI so it can request signals (e.g. SIGINT when
    /// the user presses `q`). Taken in `launch`.
    sig_tx: Option<mpsc::Sender<i32>>,
    /// Receiver drained by `signals()` and merged into the signal stream.
    sig_rx: Option<mpsc::Receiver<i32>>,
    /// Buffer caps and scrollback for the TUI. Built from the user's
    /// `[monitor]` config section by the CLI before construction.
    settings: airlock_monitor::TuiSettings,
}

impl MonitorRuntime {
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
        // The tab bar occupies rows at the bottom; the guest PTY only gets
        // the body area. Advertising the full terminal size would let the
        // guest draw past vt100's grid and collapse onto the last row.
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
        network: &Network,
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

        // Forward network events from the broadcast channel to the TUI thread.
        let net_tx = tui.tx.clone();
        let mut events = network.events();
        tokio::task::spawn_local(async move {
            loop {
                match events.recv().await {
                    Ok(ev) => net_tx.send_network(ev),
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        // Poll guest CPU/memory stats once per second and forward to the TUI.
        let stats_tx = tui.tx.clone();
        tokio::task::spawn_local(async move {
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

        Ok(MonitorTerminal { tui: Some(tui) })
    }
}

pub struct MonitorTerminal {
    tui: Option<airlock_monitor::TuiHandle>,
}

impl Terminal for MonitorTerminal {
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

    fn exit(mut self, exit_code: i32) -> i32 {
        let Some(tui) = self.tui.take() else {
            return exit_code;
        };
        tui.tx.send_exit(exit_code);
        tui.join().unwrap_or(exit_code)
    }
}
