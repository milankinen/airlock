//! The runtime an interactive sandbox uses on the user's terminal: raw
//! passthrough, or the TUI monitor with `--monitor`.

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

/// Either host runtime, chosen once per command. One concrete type keeps
/// the session code monomorphic.
pub enum HostRuntime {
    Raw(RawTerminalRuntime),
    Monitor(MonitorRuntime),
}

impl HostRuntime {
    /// The monitor runtime when `monitor` is set, else the raw terminal.
    /// Fails, before anything boots, when the `[monitor.keys]` settings
    /// are invalid; the error lists each problem.
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

/// The [`Terminal`] of a [`HostRuntime`].
pub enum HostTerminal {
    Raw(RawTerminal),
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
    use super::*;
    use crate::settings::{KeyList, MonitorBuffers, MonitorSettings, VaultSettings};

    #[test]
    fn new_rejects_bad_monitor_keys() {
        let mut settings = Settings {
            vault: VaultSettings::default(),
            monitor: MonitorSettings {
                buffers: MonitorBuffers {
                    http: 100,
                    tcp: 100,
                    scrollback: 1000,
                },
                keys: [("no-such-action".to_string(), KeyList(vec!["q".into()]))].into(),
            },
        };
        let Err(e) = HostRuntime::new(true, &settings) else {
            panic!("bad keys must be rejected");
        };
        assert!(
            e.to_string().starts_with("invalid monitor key bindings:"),
            "{e}"
        );
        // Without `--monitor` the key bindings are not used, so not checked.
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
