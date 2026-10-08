//! Terminal runtimes for interactive sandboxes.
//!
//! A runtime connects the sandbox to the user's terminal. It gives the guest
//! its input and shows the guest output. There are two runtimes: a raw
//! terminal, and the TUI monitor that also shows network events and guest
//! stats. The session uses the same code for both. Also forwards host
//! signals to the guest, and can write a copy of the guest output to a file.

use std::pin::Pin;

use airlock_common::supervisor_capnp::stdin;
use futures::{Stream, StreamExt};

mod dump;
mod host;
mod monitor_terminal;
mod raw_terminal;
mod signals;

pub use dump::DumpSink;
pub use host::HostRuntime;
pub use monitor_terminal::MonitorRuntime;
pub use raw_terminal::RawTerminalRuntime;
pub use signals::signals;

use crate::network::NetworkHandle;
use crate::project::Project;
use crate::rpc;

/// Guest terminal size `(rows, cols)` in PTY mode, or `None` in pipe mode.
pub type PtySize = Option<(u16, u16)>;
/// Stream of Linux signal numbers to forward to the guest process.
pub type SignalStream = Pin<Box<dyn Stream<Item = i32>>>;

/// Sink for guest process output.
pub trait OutputSink {
    /// Handle a chunk of bytes that the guest wrote to stdout.
    fn stdout(&mut self, bytes: &[u8]);

    /// Handle a chunk of bytes that the guest wrote to stderr.
    fn stderr(&mut self, bytes: &[u8]);
}

/// An [`OutputSink`] that owns the user's terminal and gets the exit code.
pub trait Terminal: OutputSink {
    /// Finish the terminal with the exit code of the guest process.
    /// Returns:
    ///   Exit code for airlock. The TUI can override the guest code if the
    ///   user quit the UI.
    fn exit(self, exit_code: i32) -> i32;
}

/// Makes the supervisor stdin client and the [`Terminal`] output sink.
pub trait Runtime {
    /// Output sink that [`Runtime::launch`] returns.
    type Terminal: Terminal;

    /// Make the supervisor stdin client.
    /// Returns:
    ///   Stdin client and the guest PTY size.
    fn attach_stdin(&mut self) -> anyhow::Result<(stdin::Client, PtySize)>;

    /// Get the stream of signal numbers to forward to the guest process. A
    /// runtime can merge host OS signals with signals from the TUI (for
    /// example, the monitor runtime sends SIGHUP and SIGTERM when the user
    /// stops the sandbox from the TUI).
    fn signals(&mut self) -> anyhow::Result<SignalStream>;

    /// Consume the runtime and start the output sink. Call it after setup
    /// and downloads, so Ctrl+C works during preparation.
    ///
    /// It also takes control of terminal raw mode: the raw runtime enables
    /// raw mode here, and the monitor runtime gives control to the TUI
    /// thread. The returned terminal owns the tasks that the runtime starts,
    /// and they stop with it.
    fn launch(
        self,
        project: &Project,
        network: &NetworkHandle,
        supervisor: rpc::Supervisor,
    ) -> anyhow::Result<Self::Terminal>;
}

/// Forward each signal from `signals` to `proc` until the stream ends. The
/// caller owns the task: the session starts it as a service, and
/// `airlock exec` runs it until the process exits.
pub async fn forward_signals(mut signals: SignalStream, proc: rpc::Process) {
    while let Some(signum) = signals.next().await {
        tracing::debug!("forwarding signal {signum} to VM");
        if let Err(e) = proc.signal(signum).await {
            tracing::error!("signal forward failed: {e}");
        }
    }
}
