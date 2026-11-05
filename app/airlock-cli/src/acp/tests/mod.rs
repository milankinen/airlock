//! Tests for the ACP proxy with a fake agent that runs as a local process.

mod test_agent_ends;
mod test_agent_exit;
mod test_agent_features;
mod test_agent_request_ids;
mod test_agent_shutdown;
mod test_agent_stderr;
mod test_auth_required;
mod test_blocked_agent_requests;
mod test_capabilities;
mod test_client_features;
mod test_deferred_start;
mod test_handshake_failure;
mod test_mcp_lifecycle;
mod test_mcp_remote_bridge;
mod test_mcp_rewrite;
mod test_mcp_stdio_bridge;
mod test_multi_sandbox;
mod test_output_limit;
mod test_pass_mode;
mod test_sandbox_notices;
mod test_sandbox_start_failure;
mod test_session_dirs;
mod test_session_list;
mod test_session_routing;
mod test_shared_agent;
mod test_signals;
