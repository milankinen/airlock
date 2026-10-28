//! ACP agent proxy.
//!
//! Runs an Agent Client Protocol agent in the sandbox of the project that
//! the client (editor) opens. The client can start the agent in any
//! directory. The project dir comes only in the first session request.
//! Thus the proxy replies to the handshake itself and starts the agent in
//! the sandbox when the first session starts. Later sessions must be in the
//! same sandbox. The proxy gives the agent the session dirs as guest paths.
//!
//! By default, the proxy removes the client features that let the agent
//! act on the host (host commands, host file access, login terminal, URL
//! prompts, MCP servers in the editor). It lets through only the agent
//! messages that the remaining features use, and refuses the other agent
//! requests.

pub mod filter;
mod proxy;
mod sandbox;

use std::time::Duration;

pub use proxy::{ProxyConfig, serve};
use tokio::sync::mpsc;

use crate::context::Context;
use crate::{cli, rpc};

/// Name of the proxy log file in the data dir.
pub const LOG_FILE: &str = "acp.log";

/// Maximum time from the agent start to the end of the agent handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_mins(1);

/// Time that the agent has to exit after the client closes, and again
/// after the kill.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Maximum length of one agent output line. The proxy keeps a line in
/// memory until its end. A longer line stops the agent.
const MAX_LINE_BYTES: usize = 16 * 1024 * 1024;

/// Maximum number of lines that wait for the agent input. The agent must
/// read its input. If it does not, the proxy stops it.
pub(crate) const AGENT_INPUT_LINES: usize = 1024;

/// Maximum number of agent output parts that wait for the proxy. The agent
/// output waits in the sandbox while the proxy is busy.
pub(crate) const AGENT_EVENTS: usize = 4;

/// Settings of the ACP proxy.
pub struct Options {
    /// Agent command and its arguments, as the sandbox runs them.
    pub argv: Vec<String>,
    /// Agent command as the user gave it, for messages.
    pub name: String,
    /// Environment variables (`KEY=VALUE`) for the agent.
    pub env: Vec<String>,
    /// Give all client features to the agent and refuse no requests.
    pub pass_client_capabilities: bool,
    /// In a git worktree, also look for the sandbox of the main repository.
    pub follow_git_worktree: bool,
}

/// The sandbox of a session dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Identity of the sandbox.
    pub sandbox: String,
    /// The session dir in the guest.
    pub guest_cwd: String,
}

/// Control request for a running agent.
pub enum Control {
    /// Send a Linux signal number.
    Signal(i32),
    /// Kill the agent.
    Kill,
}

/// Handle of a started agent process.
pub struct AgentProcess {
    /// Bytes for the agent stdin, at most [`AGENT_INPUT_LINES`] parts. The
    /// agent stdin closes when it drops.
    pub input: mpsc::Sender<Vec<u8>>,
    /// Output and exit of the agent, at most [`AGENT_EVENTS`] parts. The
    /// exit is the last event.
    pub events: mpsc::Receiver<rpc::ProcessEvent>,
    /// Signal and kill requests.
    pub control: mpsc::UnboundedSender<Control>,
}

/// Starts agents for session dirs.
#[allow(async_fn_in_trait)]
pub trait AgentStarter {
    /// Find the sandbox of the session dir `cwd`.
    /// Returns:
    ///   The sandbox, or error if no sandbox runs for `cwd`.
    async fn locate(&self, cwd: &str) -> anyhow::Result<Location>;

    /// Start the agent `argv` in the sandbox of the session dir `cwd`.
    /// Returns:
    ///   The sandbox and the agent, or error if no sandbox runs for `cwd`
    ///   or the start fails.
    async fn start(&self, cwd: &str, argv: &[String]) -> anyhow::Result<(Location, AgentProcess)>;
}

/// Run the proxy between the client on stdin and stdout and the agent.
/// Args:
///  - `context`: Context with the sandbox registry
///  - `options`: Proxy settings
///
/// Returns:
///   Exit code of the agent, or 0 if the client closed before an agent
///   ran.
pub async fn run(context: &Context, options: Options) -> anyhow::Result<i32> {
    let starter = sandbox::SandboxStarter {
        context,
        follow_git_worktree: options.follow_git_worktree,
        env: options.env,
    };
    let config = ProxyConfig {
        title: crate::messages::acp::agent_title(&options.name),
        version: cli::version_string(false),
        name: options.name,
        argv: options.argv,
        restrict: !options.pass_client_capabilities,
        handshake_timeout: HANDSHAKE_TIMEOUT,
        shutdown_grace: SHUTDOWN_GRACE,
        max_line_bytes: MAX_LINE_BYTES,
        signals: Some(crate::runtime::signals),
    };
    let input = tokio::io::BufReader::new(tokio::io::stdin());
    serve(
        &starter,
        &config,
        input,
        tokio::io::stdout(),
        tokio::io::stderr(),
    )
    .await
}

#[cfg(test)]
mod tests;
