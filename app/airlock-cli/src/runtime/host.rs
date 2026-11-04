//! Runtime selection for the user's terminal.
//!
//! Selects the raw terminal runtime, or the TUI monitor when the user gives
//! `--monitor`.

use airlock_common::supervisor_capnp::stdin;

use super::monitor_terminal::MonitorTerminal;
use super::raw_terminal::RawTerminal;
use super::{
    MonitorRuntime, MonitorSource, OutputSink, PtySize, RawTerminalRuntime, Runtime, SignalStream,
    Terminal,
};
use crate::approval::MonitorSlot;
use crate::settings::Settings;

/// One of the host runtimes, selected one time per command. One concrete
/// type keeps the session code monomorphic.
pub enum HostRuntime {
    /// Raw terminal passthrough.
    Raw(RawTerminalRuntime),
    /// TUI monitor.
    Monitor(MonitorRuntime),
}

impl HostRuntime {
    /// Select the runtime.
    /// Args:
    ///  - `monitor`: Use the TUI monitor. Otherwise use the raw terminal
    ///  - `settings`: User settings with the `[monitor.keys]` bindings
    ///
    /// Returns:
    ///   Runtime, or an error that lists each problem in the
    ///   `[monitor.keys]` settings. Fails before the boot starts.
    pub fn new(monitor: bool, settings: &Settings) -> anyhow::Result<Self> {
        if !monitor {
            return Ok(Self::Raw(RawTerminalRuntime::new()));
        }
        let tui = settings.monitor.tui_settings().map_err(|e| {
            anyhow::anyhow!(crate::messages::runtime::invalid_monitor_key_bindings(e))
        })?;
        Ok(Self::Monitor(MonitorRuntime::new(tui)))
    }

    /// Get the place of the approval dialog of the monitor TUI. It holds
    /// the dialog while the TUI runs. A monitor runtime needs a
    /// [`MonitorSource`] for [`Runtime::launch`].
    ///
    /// Returns:
    ///   The place, or `None` for the raw terminal.
    pub fn approval_dialog(&self) -> Option<MonitorSlot> {
        match self {
            Self::Raw(_) => None,
            Self::Monitor(m) => Some(m.approval_dialog()),
        }
    }
}

impl Runtime for HostRuntime {
    type Terminal = HostTerminal;

    fn attach_stdin(&mut self) -> anyhow::Result<(stdin::Client, PtySize)> {
        match self {
            Self::Raw(r) => r.attach_stdin(),
            Self::Monitor(m) => m.attach_stdin(),
        }
    }

    fn signals(&mut self) -> anyhow::Result<SignalStream> {
        match self {
            Self::Raw(r) => r.signals(),
            Self::Monitor(m) => m.signals(),
        }
    }

    fn launch(self, monitor: Option<MonitorSource>) -> anyhow::Result<HostTerminal> {
        Ok(match self {
            Self::Raw(r) => HostTerminal::Raw(r.launch(monitor)?),
            Self::Monitor(m) => HostTerminal::Monitor(m.launch(monitor)?),
        })
    }
}

/// [`Terminal`] of a [`HostRuntime`].
pub enum HostTerminal {
    /// Raw terminal passthrough.
    Raw(RawTerminal),
    /// TUI monitor.
    Monitor(MonitorTerminal),
}

impl OutputSink for HostTerminal {
    fn stdout(&mut self, bytes: &[u8]) {
        match self {
            Self::Raw(t) => t.stdout(bytes),
            Self::Monitor(t) => t.stdout(bytes),
        }
    }

    fn stderr(&mut self, bytes: &[u8]) {
        match self {
            Self::Raw(t) => t.stderr(bytes),
            Self::Monitor(t) => t.stderr(bytes),
        }
    }
}

impl Terminal for HostTerminal {
    fn exit(self, exit_code: i32) -> i32 {
        match self {
            Self::Raw(t) => t.exit(exit_code),
            Self::Monitor(t) => t.exit(exit_code),
        }
    }
}

#[cfg(test)]
mod tests {
    //! Tests for the selection of the host runtime.

    use super::*;
    use crate::settings::{KeyList, MonitorBuffers, MonitorSettings, VaultSettings};

    /// Test that the monitor refuses settings with an unknown key binding
    /// action, so that the user sees the error before the boot starts.
    ///   1. Ask for the monitor with a binding for an unknown action and
    ///      check the error
    ///   2. Ask for the raw terminal with the same settings and check that it
    ///      ignores the bindings
    ///   3. Remove the bindings and check that the monitor starts
    #[test]
    fn monitor_with_unknown_key_binding_is_refused() {
        let mut settings = Settings {
            vault: VaultSettings::default(),
            data_dir: None,
            monitor: MonitorSettings {
                buffers: MonitorBuffers {
                    http: 100,
                    tcp: 100,
                    scrollback: 1000,
                },
                keys: [("no-such-action".to_string(), KeyList(vec!["q".into()]))].into(),
            },
            security: crate::settings::SecuritySettings::default(),
            packs: crate::settings::PackSettings::default(),
            approvals: crate::settings::ApprovalSettings::default(),
        };
        let Err(e) = HostRuntime::new(true, &settings) else {
            panic!("bad keys must be rejected");
        };
        assert!(
            e.to_string().starts_with("invalid monitor key bindings:"),
            "{e}"
        );
        assert!(matches!(
            HostRuntime::new(false, &settings),
            Ok(HostRuntime::Raw(_))
        ));
        settings.monitor.keys.clear();
        assert!(matches!(
            HostRuntime::new(true, &settings),
            Ok(HostRuntime::Monitor(_))
        ));
    }
}
