//! Local HTTP server of the VM.
//!
//! Starts the server with all admin endpoints. The same server also serves
//! the host MCP servers. It picks the endpoints by the host name of the
//! request: `mcp.airlock` gets the boot config MCP servers,
//! `acp.mcp.airlock` gets the run-time MCP servers, and all other names
//! get the admin endpoints.

use std::sync::Arc;

use axum::Router;
use axum::extract::Request;
use axum::http::header::HOST;
use axum::routing::{get, post};
use tokio::net::TcpListener;
use tower::ServiceExt;
use tracing::{info, warn};

use super::routes;
use super::state::AdminState;
use crate::mcp::{self, Bridge, host_name};

/// Listen address. Loopback traffic does not go through the TCP proxy, so
/// container requests come here directly.
const ADMIN_ADDR: (&str, u16) = ("127.0.0.1", 80);

/// Bind the admin listen address and serve the admin and MCP routes in a
/// background task.
/// Args:
///  - `state`: Shared state for the admin route handlers
///  - `mcp`: Handle to the MCP bridge task
///
/// Returns:
///   Error if the bind fails.
pub async fn start(state: Arc<AdminState>, mcp: Bridge) -> anyhow::Result<()> {
    let listener = TcpListener::bind(ADMIN_ADDR).await?;
    info!("admin listening on {}:{}", ADMIN_ADDR.0, ADMIN_ADDR.1);

    let app = app(state, mcp);
    tokio::task::spawn_local(async move {
        if let Err(e) = axum::serve(listener, app).await {
            warn!("admin server: {e}");
        }
    });

    Ok(())
}

/// The admin routes, without a listener.
pub(crate) fn router(state: Arc<AdminState>) -> Router {
    Router::new()
        .route("/", get(routes::root::handle))
        .route(
            "/claude/hooks/pre-tool-use",
            post(routes::claude_hook_pre_tool_use::handle),
        )
        .route(
            "/claude/hooks/post-tool-use",
            post(routes::claude_hook_post_tool_use::handle),
        )
        .route(
            "/claude/hooks/post-tool-use-failure",
            post(routes::claude_hook_post_tool_use_failure::handle),
        )
        .with_state(state)
}

/// All routes of the local server, without a listener.
/// Args:
///  - `state`: Shared state for the admin route handlers
///  - `mcp`: Handle to the MCP bridge task
pub(crate) fn app(state: Arc<AdminState>, mcp: Bridge) -> Router {
    let admin = router(state);
    let config = mcp::router(mcp.clone(), false);
    let runtime = mcp::router(mcp, true);
    Router::new().fallback_service(tower::service_fn(move |req: Request| {
        // Other host names (`admin.airlock`, `localhost`, IPs) keep the
        // admin routes, as before the MCP routes existed.
        let target = match request_host(&req).as_str() {
            mcp::CONFIG_HOST => config.clone(),
            mcp::RUNTIME_HOST => runtime.clone(),
            _ => admin.clone(),
        };
        target.oneshot(req)
    }))
}

/// Get the host name of a request. HTTP/1.1 gives it in the `Host` header,
/// HTTP/2 in the URI.
fn request_host(req: &Request) -> String {
    req.headers()
        .get(HOST)
        .and_then(|h| h.to_str().ok())
        .or_else(|| {
            req.uri()
                .authority()
                .map(axum::http::uri::Authority::as_str)
        })
        .map(host_name)
        .unwrap_or_default()
}
