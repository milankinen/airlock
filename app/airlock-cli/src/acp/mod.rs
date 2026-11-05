//! ACP agent proxy.
//!
//! Runs Agent Client Protocol agents in the sandboxes of the projects that
//! the client (editor) opens. The client can start the proxy in any
//! directory, and one proxy can serve many projects. The project dir comes
//! only in the session requests. Thus the proxy replies to the handshake
//! itself and starts one agent for each sandbox, at the first session in
//! the sandbox of the session dir. All sessions in a sandbox share its
//! agent. An agent sees only the messages of its own sessions. An agent
//! exit ends only that agent and its sessions. The proxy gives the agents
//! the session dirs as guest paths. Each new or loaded session shows all
//! notices of its sandbox as warnings, for example a project config that
//! changed after the last `airlock start`.
//!
//! By default, the proxy removes the client features that let the agent
//! act on the host (host commands, host file access, login terminal, URL
//! prompts). It lets through only the agent messages that the remaining
//! features use, and refuses the other agent requests.
//!
//! The handshake reply offers a set of agent features. Each common agent
//! has a preset set, and the user can give a different set. The proxy
//! refuses the client requests for offered features that the real agent
//! does not have. It also changes the guest session dirs in the session
//! list to host paths.
//!
//! The proxy also removes the MCP servers that the editor gives for a
//! session. If the user enables it, the proxy runs or reaches these
//! servers on the host instead, and gives them to the agent as HTTP
//! servers in the sandbox. Their tools then act on the host.

mod agent;
pub mod capabilities;
mod mcp;
mod proxy;
pub mod rpc;

use std::time::Duration;

use proxy::ProxyConfig;
#[cfg(test)]
pub(crate) use proxy::{
    Claim, HandleId, HandleMessage, SessionContext, SessionFactory, SessionFactoryInit,
    SessionHandle, run as route,
};
use tokio::io::{AsyncBufRead, AsyncWrite};
use tokio::sync::mpsc;

use crate::cli;
use crate::cli::LogLevel;
use crate::context::Context;
use crate::rpc::ProcessEvent;
use crate::runtime::SignalStream;
use crate::sandbox::{McpRegistrar, NoticeReader};

/// Name of the proxy log file in the data dir.
pub const LOG_FILE: &str = "acp.log";

/// Maximum time from the agent start to the end of the agent handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_mins(1);

/// Time that the agent has to exit after the client closes, and again
/// after the kill.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Maximum time to register the editor MCP servers of one session request
/// in the sandbox.
const MCP_REGISTER_TIMEOUT: Duration = Duration::from_secs(10);

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
    /// Bridge the editor MCP servers into the sandbox. If not set, the
    /// agent gets no editor MCP servers.
    pub proxy_mcp_servers: bool,
    /// Log level of a sandbox process that the proxy starts.
    pub log_level: LogLevel,
    /// Agent feature names that the handshake reply offers.
    pub agent_capabilities: Vec<String>,
}

/// Settings of the proxy and its agents.
pub struct Config {
    /// Agent command and its arguments.
    pub argv: Vec<String>,
    /// Agent command as the user gave it, for messages.
    pub name: String,
    /// Agent name that the client shows.
    pub title: String,
    /// Version of airlock.
    pub version: String,
    /// Remove the client features that let the agent act on the host.
    pub restrict: bool,
    /// Bridge the editor MCP servers into the sandbox. If not set, the
    /// agent gets no editor MCP servers.
    pub proxy_mcp_servers: bool,
    /// Agent feature names that the handshake reply offers.
    pub agent_capabilities: Vec<String>,
    /// Maximum time to register the editor MCP servers of one session
    /// request.
    pub mcp_register_timeout: Duration,
    /// Maximum time from the agent start to the end of the handshake.
    pub handshake_timeout: Duration,
    /// Time that an agent has to exit after the proxy closes its input,
    /// and again after the kill.
    pub shutdown_grace: Duration,
    /// Source of the host signals, or `None` to not watch them.
    pub signals: Option<fn() -> anyhow::Result<SignalStream>>,
    /// Maximum length of one agent output line.
    pub max_line_bytes: usize,
}

impl Config {
    /// Split the settings into the router part and the agent part.
    fn split(self) -> (ProxyConfig, agent::AgentConfig) {
        let proxy = ProxyConfig {
            title: self.title,
            version: self.version,
            restrict: self.restrict,
            proxy_mcp_servers: self.proxy_mcp_servers,
            agent_capabilities: self.agent_capabilities,
            signals: self.signals,
        };
        let agent = agent::AgentConfig {
            argv: self.argv,
            name: self.name,
            restrict: self.restrict,
            proxy_mcp_servers: self.proxy_mcp_servers,
            mcp_register_timeout: self.mcp_register_timeout,
            handshake_timeout: self.handshake_timeout,
            shutdown_grace: self.shutdown_grace,
            max_line_bytes: self.max_line_bytes,
        };
        (proxy, agent)
    }

    /// Get the router part of the settings.
    #[cfg(test)]
    pub(crate) fn proxy(self) -> ProxyConfig {
        self.split().0
    }
}

/// The sandbox of a session dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    /// Identity of the sandbox.
    pub sandbox: String,
    /// The session dir in the guest.
    pub guest_cwd: String,
    /// Project dir on the host.
    pub host_root: String,
    /// Project dir in the guest.
    pub guest_root: String,
}

/// Control request for a running agent.
pub enum Control {
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
    pub events: mpsc::Receiver<ProcessEvent>,
    /// Kill requests.
    pub control: mpsc::UnboundedSender<Control>,
    /// Adds the editor MCP servers to the sandbox, or `None` if the
    /// sandbox cannot add them.
    pub mcp: Option<McpRegistrar>,
    /// Reads the notices of the sandbox, or `None` if the launcher cannot
    /// read them.
    pub notices: Option<NoticeReader>,
}

/// Finds the sandbox of a session dir and launches agent processes there.
#[allow(async_fn_in_trait)]
pub trait AgentProcessLauncher {
    /// Find the sandbox of the session dir `cwd`. Does not start it.
    /// Returns:
    ///   The sandbox, or error if `cwd` has no sandbox.
    async fn locate(&self, cwd: &str) -> anyhow::Result<Location>;

    /// Launch the agent process `argv` in the sandbox of the session dir
    /// `cwd`. Starts the sandbox if it does not run.
    /// Returns:
    ///   The sandbox and the agent process, or error if `cwd` has no
    ///   sandbox, or the sandbox start or the launch fails.
    async fn launch(&self, cwd: &str, argv: &[String]) -> anyhow::Result<(Location, AgentProcess)>;
}

/// Run the proxy between the client on stdin and stdout and the agent.
/// Args:
///  - `context`: Context with the sandbox registry
///  - `options`: Proxy settings
///
/// Returns:
///   The first exit code that is not 0 of the agents, else 0. 128 + the
///   signal number if a signal came before an agent completed its
///   handshake.
pub async fn run(context: &Context, options: Options) -> anyhow::Result<i32> {
    let launcher = agent::launcher::SandboxProcessLauncher {
        context: context.clone(),
        follow_git_worktree: options.follow_git_worktree,
        env: options.env,
        log_level: options.log_level,
        #[cfg(test)]
        launcher: None,
    };
    let config = Config {
        title: crate::messages::acp::agent_title(&options.name),
        version: cli::version_string(false),
        name: options.name,
        argv: options.argv,
        restrict: !options.pass_client_capabilities,
        proxy_mcp_servers: options.proxy_mcp_servers,
        agent_capabilities: options.agent_capabilities,
        mcp_register_timeout: MCP_REGISTER_TIMEOUT,
        handshake_timeout: HANDSHAKE_TIMEOUT,
        shutdown_grace: SHUTDOWN_GRACE,
        max_line_bytes: MAX_LINE_BYTES,
        signals: Some(crate::runtime::signals),
    };
    let input = tokio::io::BufReader::new(tokio::io::stdin());
    let output = tokio::io::stdout();
    Ok(serve(launcher, config, input, output, tokio::io::stderr()).await)
}

/// Run the proxy between the client and the agents that `launcher` launches.
/// Args:
///  - `launcher`: Launches the agent processes in their sandboxes
///  - `config`: Proxy settings
///  - `input`: Client messages, one per line
///  - `output`: Destination of the messages to the client
///  - `errors`: Destination of the agent stderr
///
/// Returns:
///   The exit code of the proxy, as [`proxy::run`] gives it.
pub async fn serve<S, R, W, E>(launcher: S, config: Config, input: R, output: W, errors: E) -> i32
where
    S: AgentProcessLauncher + 'static,
    R: AsyncBufRead + Unpin + 'static,
    W: AsyncWrite + Unpin + 'static,
    E: AsyncWrite + Unpin + 'static,
{
    let (proxy_config, agent_config) = config.split();
    let grace = agent_config.shutdown_grace;
    let (err, mut err_task) = proxy::spawn_writer(errors);
    let (factory, dropped) = agent::factory(launcher, agent_config, err);
    let code = proxy::run(factory, &proxy_config, input, output).await;
    if dropped.get() > 0 {
        tracing::info!(bytes = dropped.get(), "dropped agent stderr");
    }
    if tokio::time::timeout(grace, crate::mcp::deletes_done())
        .await
        .is_err()
    {
        tracing::info!("MCP server release did not complete");
    }
    // A stderr that nobody reads must not stop the exit.
    if tokio::time::timeout(grace, &mut err_task).await.is_err() {
        tracing::warn!("stderr write did not complete");
        err_task.abort();
    }
    code
}

#[cfg(test)]
mod tests;
