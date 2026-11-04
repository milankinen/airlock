//! Attached process in a sandbox.
//!
//! Runs one process in a sandbox over the CLI connection and connects it
//! to the user's terminal: input, output, terminal size, signals and the
//! exit code. The terminal can be the raw terminal or the TUI monitor.
//! Tells the user when the sandbox stops before the process exits.

use airlock_common::cli_capnp::cli_service;

use crate::runtime::{self, MonitorSource, Runtime, Terminal};
use crate::util::VerifiedPath;
use crate::{cli, messages, rpc};

/// The process to run in the sandbox.
pub struct AttachRequest<'a> {
    /// Command and its arguments.
    pub argv: &'a [String],
    /// Working directory in the guest. An empty value is the sandbox cwd.
    pub cwd: &'a str,
    /// Environment overrides (`KEY=VALUE`).
    pub env: &'a [String],
    /// Sandbox directory for a copy of the output when
    /// `AIRLOCK_PTY_DUMP=1`. `None` makes no copy.
    pub dump_dir: Option<&'a VerifiedPath>,
}

/// Run `request` in the sandbox of `service` and connect it to the
/// terminal of `runtime` until the process exits.
/// Args:
///  - `service`: CLI service of the sandbox
///  - `runtime`: Runtime of the user's terminal
///  - `monitor`: Data of the TUI monitor, if the runtime is the monitor
///  - `request`: The process to run
///
/// Returns:
///   The exit code from the terminal. Exit code 1 if the sandbox stops
///   or the connection fails before the process exits. Error if the
///   process does not start.
pub async fn run_attached<R: Runtime>(
    service: &cli_service::Client,
    mut runtime: R,
    monitor: Option<MonitorSource>,
    request: &AttachRequest<'_>,
) -> anyhow::Result<i32> {
    let (stdin, pty) = runtime.attach_stdin()?;
    let signals = runtime.signals()?;
    let mut req = service.exec_request();
    rpc::fill_exec_request(
        req.get(),
        stdin,
        pty,
        request.argv,
        request.cwd,
        request.env,
    );
    let response = req.send().promise.await?;
    let proc = rpc::Process::new(response.get()?.get_proc()?);

    // The raw runtime enters raw mode here, the monitor starts its TUI.
    let mut terminal = runtime.launch(monitor)?;
    let forward = tokio::task::spawn_local(runtime::forward_signals(signals, proc.clone()));
    let result = match request.dump_dir {
        Some(dir) => {
            let mut sink = runtime::DumpSink::from_env(&mut terminal, dir);
            super::vm::io::try_drive(&proc, &mut sink).await
        }
        None => super::vm::io::try_drive(&proc, &mut terminal).await,
    };
    forward.abort();
    match result {
        Ok(code) => Ok(terminal.exit(code)),
        Err(e) => {
            tracing::error!("process poll error: {e:#}");
            // Give the terminal back to the user before the message.
            terminal.exit(1);
            if !cli::is_silent() {
                if rpc::is_disconnected(&e) {
                    cli::error!("{}", messages::cli::SANDBOX_STOPPED);
                } else {
                    cli::error!("{}", messages::cli::process_lost(format!("{e:#}")));
                }
            }
            Ok(1)
        }
    }
}
