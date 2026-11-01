//! MCP server of the admin service.
//!
//! Serves the `airlock` MCP server at `http://admin.airlock/mcp` with the
//! MCP Streamable HTTP transport. The server is stateless and sends JSON
//! responses. Its tools let the agent:
//!  * run a shell command with extended network access, from given hosts or
//!    from an on-demand rule. The access ends when the command ends
//!  * show the network rules
//!  * add or change a network rule
//!  * read the airlock user manual
//!
//! The server offers only the tools of the features that the host enabled.
//! The user on the host must approve each command and each rule change.
//!
//! Only processes in the VM can access the endpoint. The endpoint refuses
//! requests with other host names and requests from web pages of other
//! origins (DNS rebinding guard).

mod bridge;
mod instructions;
mod manual;
mod tools;

use std::sync::Arc;

use axum::Router;
pub use bridge::{GrantBridge, GrantContext, SpawnCreds};
pub(crate) use instructions::instructions;
use rmcp::transport::streamable_http_server::session::never::NeverSessionManager;
use rmcp::transport::streamable_http_server::{StreamableHttpServerConfig, StreamableHttpService};

use super::capabilities::Capabilities;

/// Host names that the endpoint accepts in the `Host` header.
const ALLOWED_HOSTS: [&str; 3] = ["admin.airlock", "localhost", "127.0.0.1"];

/// Web page origins that the endpoint accepts. Requests with no `Origin`
/// header do not come from a web page, so they can continue.
const ALLOWED_ORIGINS: [&str; 6] = [
    "http://admin.airlock:*",
    "https://admin.airlock:*",
    "http://localhost:*",
    "https://localhost:*",
    "http://127.0.0.1:*",
    "https://127.0.0.1:*",
];

/// Make the MCP route (`/mcp`) of the admin service.
/// Args:
///  - `grants`: Handle to the grant task, or `None` if the host network
///    service is not available (the network tools then give an error)
///  - `caps`: Agent features that the host enabled
pub(crate) fn router(grants: Option<GrantBridge>, caps: Capabilities) -> Router {
    let config = StreamableHttpServerConfig::default()
        .with_legacy_session_mode(false)
        .with_json_response(true)
        .with_allowed_hosts(ALLOWED_HOSTS)
        .with_allowed_origins(ALLOWED_ORIGINS);
    let tools = tools::AdminTools::new(grants, caps);
    let service = StreamableHttpService::new(
        move || Ok(tools.clone()),
        Arc::new(NeverSessionManager::default()),
        config,
    );
    Router::new().nest_service("/mcp", service)
}
