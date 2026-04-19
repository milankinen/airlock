//! Runtime abstraction for `airlock start`: picks between the raw host
//! terminal (non-monitor) and the TUI monitor control panel, hiding the
//! branching from `cmd_start`.
//!
//! A [`Runtime`] produces a supervisor [`stdin::Client`] plus a [`Terminal`]
//! output sink. `cmd_start` then runs a single poll loop regardless of which
//! variant is in use.

use std::pin::Pin;

use airlock_common::supervisor_capnp::stdin;
use futures::Stream;

mod monitor_terminal;
mod raw_terminal;
mod signals;

pub use monitor_terminal::MonitorRuntime;
pub use raw_terminal::RawTerminalRuntime;
pub use signals::signals;

use crate::network::Network;
use crate::project::Project;
use crate::rpc;

pub type PtySize = Option<(u16, u16)>;
pub type SignalStream = Pin<Box<dyn Stream<Item = i32>>>;

/// Sink for guest process output and the exit code.
pub trait Terminal {
    /// Handle a chunk of bytes the guest wrote to stdout.
    fn stdout(&mut self, bytes: &[u8]);

    /// Handle a chunk of bytes the guest wrote to stderr.
    fn stderr(&mut self, bytes: &[u8]);

    /// Finalize the terminal with the guest process's exit code. Returns the
    /// exit code airlock should exit with (the TUI may override the guest
    /// code if the user quit the UI).
    fn exit(self, exit_code: i32) -> i32;
}

/// Builds the supervisor stdin client and the [`Terminal`] output sink.
pub trait Runtime {
    type Terminal: Terminal;

    /// Create the supervisor stdin client plus the guest PTY size.
    fn attach_stdin(&mut self) -> anyhow::Result<(stdin::Client, PtySize)>;

    /// Stream of signal numbers to forward to the guest process. Runtimes
    /// may merge host OS signals with TUI-originated signals (e.g. the
    /// monitor runtime emits SIGINT when the user presses `q`).
    fn signals(&mut self) -> anyhow::Result<SignalStream>;

    /// Consume the runtime and start the output sink. Also takes ownership of
    /// terminal raw mode — the raw runtime enables it here, the monitor
    /// runtime hands control to the TUI thread. Called after setup/downloads
    /// so that Ctrl+C works during preparation.
    fn launch(
        self,
        project: &Project,
        network: &Network,
        supervisor: rpc::Supervisor,
    ) -> anyhow::Result<Self::Terminal>;
}
