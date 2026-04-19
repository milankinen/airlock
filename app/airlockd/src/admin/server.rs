//! Axum bootstrap for the admin HTTP service.
//!
//! Binds `127.0.0.1:80` so loopback traffic from the container bypasses
//! the transparent proxy's iptables redirect and lands here directly.

use std::sync::Arc;

use axum::Router;
use axum::routing::{get, post};
use tokio::net::TcpListener;
use tracing::{info, warn};

use super::routes;
use super::state::AdminState;

const ADMIN_ADDR: (&str, u16) = ("127.0.0.1", 80);

pub async fn start(state: Arc<AdminState>) -> anyhow::Result<()> {
    let listener = TcpListener::bind(ADMIN_ADDR).await?;
    info!("admin listening on {}:{}", ADMIN_ADDR.0, ADMIN_ADDR.1);

    let app = Router::new()
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
        .with_state(state);

    tokio::task::spawn_local(async move {
        if let Err(e) = axum::serve(listener, app).await {
            warn!("admin server: {e}");
        }
    });

    Ok(())
}
