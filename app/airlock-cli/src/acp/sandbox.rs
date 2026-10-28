//! Agent start in a running sandbox.
//!
//! Finds the running sandbox of a session dir and starts the agent there,
//! the same as `airlock exec` does. Relays the agent input, output, exit,
//! signals and kill over the connection to the sandbox.

use std::cell::RefCell;
use std::rc::Rc;

use airlock_common::supervisor_capnp::stdin;
use tokio::sync::{mpsc, oneshot};

use super::{AgentProcess, AgentStarter, Control, Location};
use crate::cli::sandbox_client::{self, RunningSandbox, SandboxClient};
use crate::context::Context;
use crate::rpc;
use crate::util::verified_path::VerifiedPath;

/// Starts the agent in the running sandbox of the session dir.
pub struct SandboxStarter<'a> {
    /// Context with the sandbox registry.
    pub context: &'a Context,
    /// In a git worktree, also look for the sandbox of the main repository.
    pub follow_git_worktree: bool,
    /// Environment variables (`KEY=VALUE`) for the agent.
    pub env: Vec<String>,
}

impl SandboxStarter<'_> {
    /// Find the running sandbox of the session dir `cwd`.
    /// Returns:
    ///   The sandbox and the session dir in the guest.
    async fn find(&self, cwd: &str) -> anyhow::Result<(RunningSandbox, Location)> {
        // The client picks the session dir, the same as the user picks the
        // current dir of `airlock exec`. Thus symlinks in it are followed,
        // as for the current dir. The sandbox lookup then refuses symlinks.
        let work = VerifiedPath::new_follow(cwd, None)?;
        let running = sandbox_client::locate(self.context, &work, self.follow_git_worktree).await?;
        let location = Location {
            sandbox: running.found.id.clone(),
            guest_cwd: running.guest_dir(&work)?,
        };
        Ok((running, location))
    }
}

impl AgentStarter for SandboxStarter<'_> {
    async fn locate(&self, cwd: &str) -> anyhow::Result<Location> {
        Ok(self.find(cwd).await?.1)
    }

    async fn start(&self, cwd: &str, argv: &[String]) -> anyhow::Result<(Location, AgentProcess)> {
        let (running, location) = self.find(cwd).await?;
        let client = SandboxClient::connect_sock(&running.sock).await?;
        // The connection stays until the agent exits. A failed start drops
        // `done` and closes the connection.
        let (done, done_rx) = oneshot::channel::<()>();
        let rpc_system = client.rpc_system;
        tokio::task::spawn_local(async move {
            tokio::select! {
                _ = rpc_system => {}
                _ = done_rx => {}
            }
        });

        let (input, input_rx) = mpsc::channel(super::AGENT_INPUT_LINES);
        let mut req = client.service.exec_request();
        rpc::fill_exec_request(
            req.get(),
            capnp_rpc::new_client(ChannelStdin(RefCell::new(input_rx))),
            None,
            argv,
            &location.guest_cwd,
            &self.env,
        );
        let response = req.send().promise.await?;
        let proc = rpc::Process::new(response.get()?.get_proc()?);

        let (events_tx, events) = mpsc::channel(super::AGENT_EVENTS);
        tokio::task::spawn_local(relay_events(proc.clone(), events_tx, done));
        let (control, control_rx) = mpsc::unbounded_channel();
        tokio::task::spawn_local(relay_control(proc, control_rx));
        let agent = AgentProcess {
            input,
            events,
            control,
        };
        Ok((location, agent))
    }
}

/// Send the agent output and exit to `events` until the agent exits.
/// Drops `done` at the end.
async fn relay_events(
    proc: rpc::Process,
    events: mpsc::Sender<rpc::ProcessEvent>,
    _done: oneshot::Sender<()>,
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
                tracing::error!("agent poll error: {e}");
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

/// Stdin server that gives the agent the bytes from a channel. Gives end
/// of file when all senders drop.
struct ChannelStdin(RefCell<mpsc::Receiver<Vec<u8>>>);

impl stdin::Server for ChannelStdin {
    // Single-threaded runtime, so RefCell is correct here.
    #[allow(clippy::await_holding_refcell_ref)]
    async fn read(
        self: Rc<Self>,
        _params: stdin::ReadParams,
        mut results: stdin::ReadResults,
    ) -> Result<(), capnp::Error> {
        let data = self.0.borrow_mut().recv().await;
        let mut frame = results.get().init_input().init_stdin();
        match data {
            Some(data) => frame.set_data(&data),
            None => frame.set_eof(()),
        }
        Ok(())
    }
}
