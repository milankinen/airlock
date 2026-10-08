//! Runtime selection for the user's terminal.
//!
//! Selects the raw terminal runtime, or the TUI monitor when the user gives
//! `--monitor`.

use airlock_common::supervisor_capnp::stdin;

use super::monitor_terminal::MonitorTerminal;
use super::raw_terminal::RawTerminal;
use super::{
    MonitorRuntime, OutputSink, PtySize, RawTerminalRuntime, Runtime, SignalStream, Terminal,
};
use crate::network::NetworkHandle;
use crate::project::Project;
use crate::rpc;
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
        let tui = settings
            .monitor
            .tui_settings()
            .map_err(|e| anyhow::anyhow!("invalid monitor key bindings:\n{e}"))?;
        Ok(Self::Monitor(MonitorRuntime::new(tui)))
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

    fn launch(
        self,
        project: &Project,
        network: &NetworkHandle,
        supervisor: rpc::Supervisor,
    ) -> anyhow::Result<HostTerminal> {
        Ok(match self {
            Self::Raw(r) => HostTerminal::Raw(r.launch(project, network, supervisor)?),
            Self::Monitor(m) => HostTerminal::Monitor(m.launch(project, network, supervisor)?),
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
    use crate::settings::{
        KeyList, MonitorBuffers, MonitorSettings, VaultSettings, WizardDefaults,
    };

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
            sandbox_location: crate::settings::SandboxLocation::default(),
            data_dir: None,
            monitor: MonitorSettings {
                buffers: MonitorBuffers {
                    http: 100,
                    tcp: 100,
                    scrollback: 1000,
                },
                keys: [("no-such-action".to_string(), KeyList(vec!["q".into()]))].into(),
            },
            wizard_defaults: WizardDefaults::default(),
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
