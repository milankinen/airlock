//! TUI monitor `Runtime`: channel-backed stdin, ratatui thread, and the
//! network-event / stats-poll forwarders.

use airlock_protocol::supervisor_capnp::stdin;

use super::{PtySize, Runtime, SignalStream, Terminal};
use crate::network::Network;
use crate::project::Project;
use crate::rpc;

/// Build a TUI-backed runtime. `attach_stdin` must be called before `launch`
/// so the supervisor gets its channel-backed stdin client.
pub struct MonitorRuntime {
    stdin_tx: Option<tokio::sync::mpsc::Sender<airlock_tui::TuiInputEvent>>,
}

impl MonitorRuntime {
    pub fn new() -> Self {
        Self { stdin_tx: None }
    }
}

impl Default for MonitorRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl Runtime for MonitorRuntime {
    type Terminal = MonitorTerminal;

    fn attach_stdin(&mut self) -> anyhow::Result<(stdin::Client, PtySize)> {
        let (cols, rows) = crossterm::terminal::size().unwrap_or((80, 24));
        // The tab bar occupies rows at the bottom; the guest PTY only gets
        // the body area. Advertising the full terminal size would let the
        // guest draw past vt100's grid and collapse onto the last row.
        let body_rows = rows.saturating_sub(airlock_tui::TAB_BAR_HEIGHT);
        let (tx, rx) = tokio::sync::mpsc::channel(64);
        let tui_stdin = airlock_tui::TuiStdin::new(rx, Some((body_rows, cols)));
        let pty_size = tui_stdin.pty_size();
        self.stdin_tx = Some(tx);
        Ok((capnp_rpc::new_client(tui_stdin), pty_size))
    }

    fn signals(&self) -> anyhow::Result<SignalStream> {
        super::signals()
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
        let policy_str = format!("{:?}", project.config.network.policy).to_lowercase();
        let project_path = project.host_cwd.display().to_string();
        let tui = airlock_tui::spawn(stdin_tx, policy_str, project_path);

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
                    Ok(snap) => stats_tx.send_stats(airlock_tui::StatsSnapshot {
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
    tui: Option<airlock_tui::TuiHandle>,
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
