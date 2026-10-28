//! Host MCP servers as HTTP endpoints.
//!
//! Serves each MCP server that the host grants with the MCP Streamable HTTP
//! transport (spec version 2025-06-18). There are two kinds of servers:
//!  * Servers from the boot config, at `http://mcp.airlock/<name>`. They
//!    stay for the life of the VM.
//!  * Servers that the host adds at run time, at
//!    `http://acp.mcp.airlock/<session>/<name>`. Each stays until the host
//!    releases its grant.
//!
//! The server itself runs on the host. This side relays the JSON-RPC
//! messages without change. Concepts:
//!  * Each HTTP session starts its own server process on the host. Thus the
//!    sessions are isolated, and request IDs need no change.
//!  * A request gets its reply as an event stream. The stream also carries
//!    the server messages that arrive while the request is open.
//!  * A client can open one more stream per session for server messages
//!    that arrive while no request is open.
//!  * The session ends when the client deletes it, when the server process
//!    stops, or when the host removes the server.
//!
//! Only processes in the VM can access the endpoints. The endpoints refuse
//! requests from web pages of other origins (DNS rebinding guard).

mod bridge;
mod routes;

#[cfg(test)]
mod tests;

use std::cell::Cell;
use std::rc::Rc;

use airlock_common::supervisor_capnp::{mcp_grant, mcp_host};
use axum::Router;
pub use bridge::{Bridge, Route, Routes, start};
use tracing::info;

/// Host name of the boot config servers.
pub const CONFIG_HOST: &str = "mcp.airlock";

/// Host name of the run-time servers.
pub const RUNTIME_HOST: &str = "acp.mcp.airlock";

/// Host MCP server grant received in `Supervisor.boot()`.
pub struct McpConfig {
    /// `None` when the host grants no MCP servers for this boot.
    pub host: Option<mcp_host::Client>,
    /// Names of the servers that the host grants.
    pub names: Vec<String>,
}

/// Make the HTTP routes of one MCP host name. Routes that have no server
/// give HTTP 404.
/// Args:
///  - `bridge`: Handle to the bridge task
///  - `runtime`: True for the routes of [`RUNTIME_HOST`], false for the
///    routes of [`CONFIG_HOST`]
pub fn router(bridge: Bridge, runtime: bool) -> Router {
    routes::router(bridge, runtime)
}

/// Add a run-time server and make its grant.
/// Args:
///  - `routes`: Route handle of the bridge
///  - `session`, `name`: Parts of the route
///    `http://acp.mcp.airlock/<session>/<name>`
///  - `host`: Host capability that starts the server processes
///
/// Returns:
///   Grant that removes the server when the host releases or drops it.
///   Error if the parts are bad, the route exists or the limit is reached.
pub async fn add_server(
    routes: &Routes,
    session: &str,
    name: &str,
    host: mcp_host::Client,
) -> Result<mcp_grant::Client, capnp::Error> {
    let (route, id) = routes
        .add(session, name, host)
        .await
        .map_err(|e| capnp::Error::failed(format!("addMcpServer: {e}")))?;
    Ok(capnp_rpc::new_client(Grant {
        routes: routes.clone(),
        route,
        id,
        released: Cell::new(false),
    }))
}

/// Lifetime token of one run-time server.
struct Grant {
    routes: Routes,
    route: Route,
    /// Registration ID. A late drop must not remove a newer server of the
    /// same route.
    id: u64,
    /// True after the first `release`.
    released: Cell<bool>,
}

impl mcp_grant::Server for Grant {
    async fn release(
        self: Rc<Self>,
        _params: mcp_grant::ReleaseParams,
        _results: mcp_grant::ReleaseResults,
    ) -> Result<(), capnp::Error> {
        if !self.released.replace(true) {
            info!("mcp: {}: grant released", self.route);
            // An error means that the bridge stopped. Then no session is
            // left.
            let _ = self.routes.remove(self.route.clone(), self.id).await;
        }
        Ok(())
    }
}

impl Drop for Grant {
    fn drop(&mut self) {
        if !self.released.get() {
            info!("mcp: {}: grant dropped", self.route);
            drop(self.routes.remove(self.route.clone(), self.id));
        }
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
