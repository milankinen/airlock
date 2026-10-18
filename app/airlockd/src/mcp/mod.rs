//! Host MCP servers as HTTP endpoints.
//!
//! Serves each MCP server that the host grants at `http://mcp.airlock/<name>`
//! with the MCP Streamable HTTP transport (spec version 2025-06-18). The
//! server itself runs on the host and talks JSON-RPC on its standard I/O.
//! This side relays the JSON-RPC messages without change. Concepts:
//!  * Each HTTP session starts its own server process on the host. Thus the
//!    sessions are isolated, and request IDs need no change.
//!  * A request gets its reply as an event stream. The stream also carries
//!    the server messages that arrive while the request is open.
//!  * A client can open one more stream per session for server messages
//!    that arrive while no request is open.
//!  * The session ends when the client deletes it, or when the server
//!    process stops.
//!
//! Only processes in the VM can access the endpoints. The endpoints refuse
//! requests from web pages of other origins (DNS rebinding guard).

mod bridge;
mod routes;

#[cfg(test)]
mod tests;

use airlock_common::supervisor_capnp::mcp_host;
use axum::Router;

/// Host MCP server grant received in `Supervisor.boot()`.
pub struct McpConfig {
    /// `None` when the host grants no MCP servers for this boot.
    pub host: Option<mcp_host::Client>,
    /// Names of the servers that the host grants.
    pub names: Vec<String>,
}

/// Make the HTTP routes of `mcp.airlock`. Without a grant, all routes give
/// HTTP 404. Call inside a `LocalSet`.
/// Args:
///  - `cfg`: MCP grant from the host
pub fn router(cfg: McpConfig) -> Router {
    match cfg.host {
        Some(host) => routes::router(bridge::Bridge::start(host), cfg.names),
        None => Router::new(),
    }
}

/// Get the host name of an HTTP authority (`host[:port]`). The result is
/// in lowercase, with no port and no trailing dot.
pub(crate) fn host_name(authority: &str) -> String {
    let host = match authority.strip_prefix('[') {
        // IPv6 literal: the port comes after the closing bracket.
        Some(rest) => rest.split(']').next().unwrap_or(""),
        None => authority.split(':').next().unwrap_or(""),
    };
    host.trim_end_matches('.').to_ascii_lowercase()
}
