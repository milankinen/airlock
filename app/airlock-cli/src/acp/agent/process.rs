//! One running agent process.
//!
//! Gives messages to the agent input and stops the agent. An agent that
//! does not read its input, or that goes over a limit, is killed. Its
//! output and input are then dropped until it exits. Also checks the agent
//! handshake reply, picks the method that opens a known session again, and
//! keeps the errors of a failed agent start.
//!
//! The trace log shows the messages in both directions. It does not show
//! the secret values of the editor MCP servers. The start failure log also
//! hides these values.

use futures::stream::LocalBoxStream;
use serde_json::{Value, json};
use tokio::sync::mpsc::error::TrySendError;
use tokio::sync::{mpsc, oneshot};

use super::super::{AgentProcess, Location, capabilities, rpc};
use super::Stderr;
use super::output::{self, Output};
use crate::messages;
use crate::sandbox::{ExitedBeforeReady, McpRegistrar, NoticeReader};

/// One running agent process.
pub(super) struct Agent {
    /// Sandbox and guest dirs of the agent.
    pub(super) location: Location,
    /// Agent input, or `None` after the input closed.
    input: Option<mpsc::Sender<Vec<u8>>>,
    /// Kills the agent, or `None` after a kill request.
    kill: Option<oneshot::Sender<()>>,
    /// Adds the editor MCP servers to the sandbox, or `None` if the
    /// sandbox cannot add them.
    pub(super) mcp: Option<McpRegistrar>,
    /// Reads the notices of the sandbox, or `None` if the launcher cannot
    /// read them.
    pub(super) notices: Option<NoticeReader>,
    /// Output lines and exit of the agent.
    pub(super) output: LocalBoxStream<'static, Output>,
    /// `initialize` result of the agent, after the handshake.
    pub(super) init: Option<Value>,
    /// The output and input of the agent are dropped until it exits: it
    /// went over a limit, or its reopen failed.
    muted: bool,
}

impl Agent {
    /// Make the agent of the launched process `process`.
    /// Args:
    ///  - `location`: Sandbox and guest dirs of the agent
    ///  - `process`: The launched agent process
    ///  - `max_line_bytes`: Maximum length of one output line
    ///  - `stderr`: Destination of the agent stderr
    pub(super) fn new(
        location: Location,
        process: AgentProcess,
        max_line_bytes: usize,
        stderr: Stderr,
    ) -> Self {
        let output = output::output(
            process.events,
            max_line_bytes,
            stderr,
            location.sandbox.clone(),
        );
        Self {
            location,
            input: Some(process.input),
            kill: Some(process.kill),
            mcp: process.mcp,
            notices: process.notices,
            output,
            init: None,
            muted: false,
        }
    }

    /// Get the sandbox of the agent.
    pub(super) fn sandbox(&self) -> &str {
        &self.location.sandbox
    }

    /// Check if the output of the agent is not for the client.
    pub(super) fn muted(&self) -> bool {
        self.muted
    }

    /// Drop the later output and input of the agent.
    pub(super) fn mute(&mut self) {
        self.muted = true;
    }

    /// Send the message `msg` to the agent. Does nothing after a limit or
    /// after the input closed. Kills the agent if its input queue is full.
    pub(super) fn send(&mut self, msg: &Value) {
        if self.muted {
            return;
        }
        let Some(input) = &self.input else {
            return;
        };
        let line = rpc::to_line(msg);
        if tracing::enabled!(tracing::Level::TRACE) {
            trace(
                self.sandbox(),
                "airlock",
                &rpc::to_line(&hide_mcp_secrets(msg)),
            );
        }
        // The agent must read its input. A full queue thus stops the
        // agent, so that the proxy does not keep lines without limit.
        if let Err(TrySendError::Full(_)) = input.try_send(line) {
            self.kill_for_limit(messages::acp::AGENT_INPUT_FULL);
        }
    }

    /// Kill the agent. Does nothing if a kill request went to it before.
    pub(super) fn kill(&mut self) {
        if let Some(kill) = self.kill.take() {
            let _ = kill.send(());
        }
    }

    /// Kill the agent because it went over a limit, and drop its later
    /// output and input.
    /// Args:
    ///  - `message`: Error text of the limit, for the log
    pub(super) fn kill_for_limit(&mut self, message: &str) {
        tracing::warn!(sandbox = self.sandbox(), "stop the agent: {message}");
        self.muted = true;
        self.kill();
    }

    /// Close the agent input. The agent exits at the end of its input.
    pub(super) fn close_input(&mut self) {
        self.input = None;
    }
}

/// Check the agent reply `reply` to the handshake request.
/// Args:
///  - `reply`: Agent response to the `initialize` request
///  - `offered`: `initialize` result that the proxy gave to the client
///  - `sandbox`: Sandbox of the agent, for the log
///
/// Returns:
///   The `initialize` result of the agent, or the start failure if the
///   agent refused the handshake or uses another ACP version.
pub(super) fn check_handshake(
    reply: &Value,
    offered: &Value,
    sandbox: &str,
) -> Result<Value, StartFailure> {
    if let Some(err) = reply.get("error") {
        let message = messages::acp::agent_handshake_failed(err);
        return Err(StartFailure::from_message(&message));
    }
    let result = reply.get("result").cloned().unwrap_or(Value::Null);
    let version = result.get("protocolVersion").unwrap_or(&Value::Null);
    if version.as_u64() != Some(rpc::PROTOCOL_VERSION) {
        let message = messages::acp::unsupported_protocol_version(version, rpc::PROTOCOL_VERSION);
        return Err(StartFailure::from_message(&message));
    }
    // The log helps to keep the presets true to the real agents.
    let missing = capabilities::missing(offered, &result);
    if !missing.is_empty() {
        tracing::info!(
            sandbox,
            ?missing,
            "the agent does not have offered features"
        );
    }
    tracing::info!(sandbox, "agent handshake complete");
    Ok(result)
}

/// Pick the method that opens a known session again, from the agent
/// features in the `initialize` result `handshake`. A resume does not send
/// the history again.
/// Returns:
///   The method, or `None` if the agent can neither resume nor load.
pub(super) fn reopen_method(handshake: &Value) -> Option<&'static str> {
    if capabilities::present(handshake, "sessionCapabilities.resume") {
        Some("session/resume")
    } else if capabilities::present(handshake, "loadSession") {
        Some("session/load")
    } else {
        None
    }
}

/// Error of a failed agent start, for the waiting session requests.
pub(super) struct StartFailure {
    /// Error message.
    pub(super) message: String,
    /// Error data, or `None` if the error has none.
    data: Option<Value>,
}

impl StartFailure {
    /// Get a copy of the failure with the error message `message`.
    pub(super) fn with_message(&self, message: String) -> Self {
        Self {
            message,
            data: self.data.clone(),
        }
    }

    /// Get the failure with the error message `message` and no data.
    pub(super) fn from_message(message: &str) -> Self {
        Self {
            message: message.to_string(),
            data: None,
        }
    }

    /// Get the failure of the agent launch error `error`. A sandbox process
    /// that stopped in its init gives the last line of its error output as
    /// the message, and its exit code and output as the data. The editor
    /// can then show why the sandbox did not start.
    pub(super) fn from_error(error: &anyhow::Error) -> Self {
        let Some(exited) = error.downcast_ref::<ExitedBeforeReady>() else {
            return Self {
                message: format!("{error:#}"),
                data: None,
            };
        };
        let message = exited
            .last_error
            .clone()
            .unwrap_or_else(|| format!("{error:#}"));
        let data = json!({
            "exitCode": exited.code,
            "output": exited.output.clone().unwrap_or_default(),
        });
        Self {
            message,
            data: Some(data),
        }
    }

    /// Make the error response to the client request `req_id`.
    pub(super) fn response(&self, req_id: &Value) -> Value {
        match &self.data {
            Some(data) => {
                rpc::error_response_with_data(req_id, rpc::INTERNAL_ERROR, &self.message, data)
            }
            None => rpc::error_response(req_id, rpc::INTERNAL_ERROR, &self.message),
        }
    }
}

/// Log a message at the trace level, as `<from>: <line>`, with the
/// sandbox as a field.
pub(super) fn trace(sandbox: &str, from: &str, line: &[u8]) {
    let line = line.strip_suffix(b"\n").unwrap_or(line);
    // The line can have control characters. The debug form escapes them,
    // so that the line cannot fake log lines.
    tracing::trace!(sandbox, "{from}: {:?}", String::from_utf8_lossy(line));
}

/// Text that replaces a secret value in the log.
const HIDDEN: &str = "***";

/// Get a copy of `msg` where the env and header values of each editor MCP
/// server are [`HIDDEN`].
pub(super) fn hide_mcp_secrets(msg: &Value) -> Value {
    let mut msg = msg.clone();
    let Some(servers) = msg
        .pointer_mut("/params/mcpServers")
        .and_then(Value::as_array_mut)
    else {
        return msg;
    };
    for server in servers {
        for field in ["env", "headers"] {
            match server.get_mut(field) {
                Some(Value::Array(items)) => {
                    for value in items.iter_mut().filter_map(|i| i.get_mut("value")) {
                        *value = HIDDEN.into();
                    }
                }
                // Not valid ACP, but the values can still be secrets.
                Some(Value::Object(map)) => map.values_mut().for_each(|v| *v = HIDDEN.into()),
                _ => {}
            }
        }
    }
    msg
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    /// Test that the trace log of a client session request does not show
    /// the env and header values of the editor MCP servers. They can be
    /// API keys and tokens.
    ///   1. Make a session request with a stdio server with env and a
    ///      remote server with headers
    ///   2. Check that the values are hidden and the names stay
    #[test]
    fn session_request_trace_hides_env_and_header_values() {
        let request = json!({
            "jsonrpc": "2.0", "id": 1, "method": "session/new",
            "params": {"cwd": "/p", "mcpServers": [
                {"name": "a", "command": "x", "args": [],
                 "env": [{"name": "API_KEY", "value": "secret-env"}]},
                {"type": "http", "name": "b", "url": "https://h/mcp",
                 "headers": [{"name": "Authorization", "value": "secret-header"}]},
            ]},
        });
        let hidden = hide_mcp_secrets(&request);
        assert!(!hidden.to_string().contains("secret"), "{hidden}");
        let servers = &hidden["params"]["mcpServers"];
        assert_eq!(
            servers[0]["env"][0],
            json!({"name": "API_KEY", "value": HIDDEN})
        );
        assert_eq!(servers[1]["headers"][0]["name"], "Authorization");
    }
}
