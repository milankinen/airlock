//! The `execute` tool: run a shell command with extended network access.
//!
//! Checks the command, the environment and the requested access, then asks
//! the host for a grant. The access ends when the command ends.

use rmcp::handler::server::wrapper::{Json, Parameters};
use rmcp::model::CallToolResult;
use rmcp::{schemars, tool, tool_router};
use serde::Deserialize;
use tokio_util::sync::CancellationToken;

use super::{AdminTools, tool_error};
use crate::admin::mcp::bridge::{ExecOutput, ExecRequest};

/// Description of the `execute` tool for the agent.
const EXECUTE: &str = "\
Request command execution with extended network access. Requested access is \
shown to the user for approval. Give either `hosts` (ad hoc) or the name of \
a pre-configured `rule`. `hosts: [\"*\"]` asks for all public hosts. \
The result `hosts` lists the hosts that the command \
connected to and if the sandbox blocked them. `aborted` is true if the user \
stopped the command.";

/// Environment variables that the command cannot set. These keys can make the
/// shell, the dynamic loader or an interpreter run other code, or send the
/// traffic to another proxy.
const REFUSED_ENV: [&str; 33] = [
    "GIT_SSH_COMMAND",
    "GIT_ASKPASS",
    "GIT_EXEC_PATH",
    "NODE_PATH",
    "LESSOPEN",
    "CDPATH",
    "BASH_XTRACEFD",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "CURL_CA_BUNDLE",
    "NODE_EXTRA_CA_CERTS",
    "REQUESTS_CA_BUNDLE",
    "BASH_ENV",
    "ENV",
    "SHELLOPTS",
    "BASHOPTS",
    "PS4",
    "PROMPT_COMMAND",
    "IFS",
    "GCONV_PATH",
    "PATH",
    "HOSTALIASES",
    "LOCPATH",
    "NODE_OPTIONS",
    "PYTHONSTARTUP",
    "PYTHONPATH",
    "PYTHONHOME",
    "PERL5OPT",
    "PERL5LIB",
    "RUBYOPT",
    "RUBYLIB",
    "JAVA_TOOL_OPTIONS",
    "_JAVA_OPTIONS",
];

/// Key prefixes that the command cannot set (dynamic loader, exported
/// shell functions and git config).
const REFUSED_ENV_PREFIXES: [&str; 3] = ["LD_", "BASH_FUNC_", "GIT_CONFIG_"];

// The schema texts are on one line. A wrapped doc comment puts a line
// break into the schema that the agent sees.

/// Arguments of `execute`.
#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ExecuteArgs {
    #[schemars(description = "Shell command to run. It runs with `sh -c`.")]
    pub cmd: String,
    #[schemars(description = "Working directory. The default is the sandbox working directory.")]
    #[serde(default)]
    pub cwd: Option<String>,
    #[schemars(
        description = "More environment variables as `KEY=VALUE`. Keys that change program start, code loading, certificates or proxies (e.g. `PATH`, `LD_*`, `*_PROXY`) are refused."
    )]
    #[serde(default)]
    pub env: Vec<String>,
    #[schemars(
        description = "Hosts to allow, as `host` or `host:port` (`*.suffix` allowed). Only `[\"*\"]` allows all public hosts. Do not give with `rule`."
    )]
    #[serde(default)]
    pub hosts: Option<Vec<String>>,
    #[schemars(description = "Name of a pre-configured rule to use. Do not give with `hosts`.")]
    #[serde(default)]
    pub rule: Option<String>,
}

#[tool_router(router = execute_router, vis = "pub(super)")]
impl AdminTools {
    /// Run a shell command with extended network access.
    #[tool(name = "execute", description = EXECUTE)]
    async fn execute(
        &self,
        Parameters(args): Parameters<ExecuteArgs>,
        ct: CancellationToken,
    ) -> Result<Json<ExecOutput>, CallToolResult> {
        let grants = self.grants()?;
        let req = check(args).map_err(|e| tool_error(&e))?;
        // A client disconnect cancels the token. Then the reply receiver
        // drops, and the grant task stops the command.
        let result = tokio::select! {
            result = grants.execute(req) => result,
            () = ct.cancelled() => Err("The request was cancelled.".into()),
        };
        result.map(Json).map_err(|e| tool_error(&e))
    }
}

/// Check the `execute` arguments.
///
/// Returns:
///   Request for the grant task, or error text for the agent.
fn check(args: ExecuteArgs) -> Result<ExecRequest, String> {
    if args.cmd.trim().is_empty() {
        return Err("The `cmd` is empty.".into());
    }
    for entry in &args.env {
        check_env(entry)?;
    }
    let hosts: Vec<String> = args
        .hosts
        .unwrap_or_default()
        .iter()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
        .collect();
    let rule = args.rule.unwrap_or_default().trim().to_string();
    if hosts.is_empty() == rule.is_empty() {
        return Err("Give either `hosts` or `rule`, not both and not neither.".into());
    }
    Ok(ExecRequest {
        cmd: args.cmd,
        env: args.env,
        cwd: args.cwd.filter(|c| !c.is_empty()),
        hosts,
        rule,
    })
}

/// Check one `KEY=VALUE` environment entry.
fn check_env(entry: &str) -> Result<(), String> {
    let Some((key, _)) = entry.split_once('=') else {
        return Err(format!(
            "The env entry `{entry}` has no `=`. Use `KEY=VALUE`."
        ));
    };
    let valid = key
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if !valid {
        return Err(format!("The env key `{key}` is not valid."));
    }
    // Proxy keys (`HTTPS_PROXY`, `no_proxy`, ...) and npm config keys are
    // refused in all letter cases.
    let upper = key.to_ascii_uppercase();
    if REFUSED_ENV.contains(&key)
        || REFUSED_ENV_PREFIXES.iter().any(|p| key.starts_with(p))
        || upper.ends_with("_PROXY")
        || upper.starts_with("NPM_CONFIG_")
    {
        return Err(format!("The env key `{key}` is not permitted."));
    }
    Ok(())
}
