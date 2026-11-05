//! Agent process launch in a sandbox.
//!
//! Finds the sandbox of a session dir and launches the agent there, the
//! same as `airlock exec` does. Starts the sandbox if it does not run. The
//! agent process holds the sandbox until it exits. Relays the agent input,
//! output, exit and kill over the connection to the sandbox. The same
//! connection adds the editor MCP servers to the sandbox and reads the
//! notices of the sandbox. Never writes to
//! the standard output, because it has the protocol.

use tokio::sync::mpsc;

use crate::acp::{AgentProcess, AgentProcessLauncher, Control, Location, NoSandbox};
use crate::cli::LogLevel;
use crate::context::Context;
use crate::rpc;
use crate::sandbox::{self, ExecRequest, FindOpts, Run, Sandbox, SandboxProcess};
use crate::util::verified_path::VerifiedPath;

/// Launches the agent process in the sandbox of the session dir.
pub struct SandboxProcessLauncher {
    /// Context with the sandbox registry.
    pub context: Context,
    /// In a git worktree, also look for the sandbox of the main repository.
    pub follow_git_worktree: bool,
    /// Environment variables (`KEY=VALUE`) for the agent.
    pub env: Vec<String>,
    /// Log level of a sandbox process that a launch starts.
    pub log_level: LogLevel,
    /// Starts the sandbox process in place of this airlock binary.
    #[cfg(test)]
    pub launcher: Option<sandbox::process::client::ProcessLauncher>,
}

impl SandboxProcessLauncher {
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
        let Some(sandbox) = sandbox::find(&self.context, &work, opts).await? else {
            return Err(NoSandbox(work.to_string()).into());
        };
        Ok((sandbox, work))
    }

    /// Get the sandbox process of `sandbox` for the agent. Starts the
    /// sandbox if it does not run.
    async fn run(&self, sandbox: Sandbox) -> anyhow::Result<SandboxProcess> {
        let run = Run::Acp {
            log_level: self.log_level,
        };
        #[cfg(test)]
        if let Some(launcher) = &self.launcher {
            return sandbox
                .run_with(
                    &self.context,
                    run,
                    launcher,
                    sandbox::process::client::TRY_TIMEOUT,
                )
                .await;
        }
        sandbox.run(&self.context, run).await
    }
}

/// Make the location of a session dir.
/// Args:
///  - `id`: Registry id of the sandbox
///  - `project`: Project dir on the host
///  - `guest_cwd`: Session dir in the guest
///  - `guest_root`: Project dir in the guest
fn location(
    id: String,
    project: &VerifiedPath,
    guest_cwd: String,
    guest_root: String,
) -> anyhow::Result<Location> {
    Ok(Location {
        sandbox: id,
        guest_cwd,
        host_root: project.to_path()?.to_string_lossy().into_owned(),
        guest_root,
    })
}

impl AgentProcessLauncher for SandboxProcessLauncher {
    async fn locate(&self, cwd: &str) -> anyhow::Result<Location> {
        let (sandbox, work) = self.find(cwd).await?;
        location(
            sandbox.id().to_string(),
            sandbox.project()?,
            sandbox.guest_dir(&work)?,
            sandbox.guest_root()?,
        )
    }

    async fn launch(&self, cwd: &str, argv: &[String]) -> anyhow::Result<(Location, AgentProcess)> {
        let (sandbox, work) = self.find(cwd).await?;
        let id = sandbox.id().to_string();
        let project = sandbox.project()?.clone();
        let process = self.run(sandbox).await?;
        // A start that ran at the same time can change the guest cwd. Thus
        // get the guest paths after the connect.
        let location = location(
            id,
            &project,
            process.guest_dir(&work)?,
            process.guest_root()?,
        )?;
        let (input, input_rx) = mpsc::channel(crate::acp::AGENT_INPUT_LINES);
        let request = ExecRequest {
            argv,
            cwd: &location.guest_cwd,
            env: &self.env,
        };
        let proc = process.exec_piped(&request, input_rx).await?;
        // The editor MCP servers go to the sandbox over the connection of
        // the agent. They thus end at the latest when the agent exits.
        let mcp = Some(process.mcp_registrar());
        let notices = Some(process.notice_reader());
        let (events_tx, events) = mpsc::channel(crate::acp::AGENT_EVENTS);
        // The connection holds the sandbox until the agent exits. A failed
        // launch drops it. All sessions of the sandbox share this agent, so
        // the sandbox runs while one of them is open.
        tokio::task::spawn_local(relay_events(proc.clone(), events_tx, process));
        let (control, control_rx) = mpsc::unbounded_channel();
        tokio::task::spawn_local(relay_control(proc, control_rx));
        let agent = AgentProcess {
            input,
            events,
            control,
            mcp,
            notices,
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

/// Do the kill requests of the proxy.
async fn relay_control(proc: rpc::Process, mut control: mpsc::UnboundedReceiver<Control>) {
    while let Some(request) = control.recv().await {
        let result = match request {
            Control::Kill => proc.kill().await,
        };
        if let Err(e) = result {
            tracing::debug!("agent control: {e}");
        }
    }
}
