//! Agent start in a sandbox.
//!
//! Finds the sandbox of a session dir and starts the agent there, the same
//! as `airlock exec` does. Starts the sandbox if it does not run. The agent
//! holds the sandbox until it exits. Relays the agent input, output, exit,
//! signals and kill over the connection to the sandbox. The same connection
//! adds the editor MCP servers to the sandbox. Never writes to the standard
//! output, because it has the protocol.

use tokio::sync::mpsc;

use super::{AgentProcess, AgentStarter, Control, Location};
use crate::cli::LogLevel;
use crate::context::Context;
use crate::rpc;
use crate::sandbox::{self, ExecRequest, FindOpts, Run, Sandbox, SandboxProcess};
use crate::util::verified_path::VerifiedPath;

/// Starts the agent in the sandbox of the session dir.
pub struct SandboxStarter<'a> {
    /// Context with the sandbox registry.
    pub context: &'a Context,
    /// In a git worktree, also look for the sandbox of the main repository.
    pub follow_git_worktree: bool,
    /// Environment variables (`KEY=VALUE`) for the agent.
    pub env: Vec<String>,
    /// Log level of a sandbox process that the start starts.
    pub log_level: LogLevel,
    /// Starts the sandbox process in place of this airlock binary.
    #[cfg(test)]
    pub launcher: Option<sandbox::process::client::ProcessLauncher>,
}

impl SandboxStarter<'_> {
    /// Find the sandbox of the session dir `cwd`. Does not start it.
    /// Returns:
    ///   The sandbox and the verified session dir on the host.
    async fn find(&self, cwd: &str) -> anyhow::Result<(Sandbox, VerifiedPath)> {
        // The client picks the session dir, the same as the user picks the
        // current dir of `airlock exec`. Thus symlinks in it are followed,
        // as for the current dir. The sandbox lookup then refuses symlinks.
        let work = VerifiedPath::new_follow(cwd, None)?;
        let opts = FindOpts {
            parents: true,
            follow_git_worktree: self.follow_git_worktree,
        };
        let Some(sandbox) = sandbox::find(self.context, &work, opts).await? else {
            anyhow::bail!(crate::messages::cli::never_started(&work));
        };
        Ok((sandbox, work))
    }

    /// Get the sandbox process of `sandbox` for the agent.
    async fn run(&self, sandbox: Sandbox) -> anyhow::Result<SandboxProcess> {
        let run = Run::Acp {
            log_level: self.log_level,
        };
        #[cfg(test)]
        if let Some(launcher) = &self.launcher {
            return sandbox
                .run_with(
                    self.context,
                    run,
                    launcher,
                    sandbox::process::client::TRY_TIMEOUT,
                )
                .await;
        }
        sandbox.run(self.context, run).await
    }
}

impl AgentStarter for SandboxStarter<'_> {
    async fn locate(&self, cwd: &str) -> anyhow::Result<Location> {
        let (sandbox, work) = self.find(cwd).await?;
        Ok(Location {
            sandbox: sandbox.id().to_string(),
            guest_cwd: sandbox.guest_dir(&work)?,
        })
    }

    async fn start(&self, cwd: &str, argv: &[String]) -> anyhow::Result<(Location, AgentProcess)> {
        let (sandbox, work) = self.find(cwd).await?;
        let id = sandbox.id().to_string();
        let process = self.run(sandbox).await?;
        // A start that ran at the same time can change the guest cwd. Thus
        // get the guest dir after the connect.
        let location = Location {
            sandbox: id,
            guest_cwd: process.guest_dir(&work)?,
        };
        let (input, input_rx) = mpsc::channel(super::AGENT_INPUT_LINES);
        let request = ExecRequest {
            argv,
            cwd: &location.guest_cwd,
            env: &self.env,
        };
        let proc = process.exec_piped(&request, input_rx).await?;
        // The editor MCP servers go to the sandbox over the connection of
        // the agent. They thus end at the latest when the agent exits.
        let mcp = Some(process.mcp_registrar());
        let (events_tx, events) = mpsc::channel(super::AGENT_EVENTS);
        // The connection holds the sandbox until the agent exits. A failed
        // start drops it.
        tokio::task::spawn_local(relay_events(proc.clone(), events_tx, process));
        let (control, control_rx) = mpsc::unbounded_channel();
        tokio::task::spawn_local(relay_control(proc, control_rx));
        let agent = AgentProcess {
            input,
            events,
            control,
            mcp,
        };
        Ok((location, agent))
    }
}

/// Send the agent output and exit to `events` until the agent exits.
/// Drops `_process` at the end.
async fn relay_events(
    proc: rpc::Process,
    events: mpsc::Sender<rpc::ProcessEvent>,
    _process: SandboxProcess,
) {
    // The channel has a small limit. A slow client thus stops the poll,
    // and the agent output waits in the guest, not in host memory. Poll
    // until the exit, also when the proxy no longer reads the events. Then
    // a kill request of the proxy can complete on the connection.
    let mut open = true;
    loop {
        let event = match proc.poll().await {
            Ok(event) => event,
            Err(e) => {
                if rpc::is_disconnected(&e) {
                    tracing::error!("{}", crate::messages::cli::SANDBOX_STOPPED);
                } else {
                    tracing::error!("agent poll error: {e:#}");
                }
                rpc::ProcessEvent::Exit(1)
            }
        };
        let exit = matches!(event, rpc::ProcessEvent::Exit(_));
        if open && events.send(event).await.is_err() {
            open = false;
        }
        if exit {
            return;
        }
    }
}

/// Do the signal and kill requests of the proxy.
async fn relay_control(proc: rpc::Process, mut control: mpsc::UnboundedReceiver<Control>) {
    while let Some(request) = control.recv().await {
        let result = match request {
            Control::Signal(signum) => proc.signal(signum).await,
            Control::Kill => proc.kill().await,
        };
        if let Err(e) = result {
            tracing::debug!("agent control: {e}");
        }
    }
}
