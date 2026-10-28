//! Fake guest supervisor for the `addMcpServer` call.

use std::rc::Rc;

use airlock_common::supervisor_capnp::{mcp_grant, mcp_host, supervisor};
use tokio::sync::mpsc;

/// One route that the fake guest got from `addMcpServer`.
pub struct Route {
    pub session: String,
    pub name: String,
    /// Host object of the route. The fake guest calls `open` on it.
    pub host: mcp_host::Client,
}

/// What happened to a grant that the fake guest returned.
#[derive(Debug, PartialEq, Eq)]
pub enum GrantEvent {
    /// The client called `release`.
    Released,
    /// The last reference to the grant is gone.
    Dropped,
}

/// Guest supervisor with only `addMcpServer`. It records the routes and
/// reports the release and drop of each grant.
pub struct McpGuest {
    routes: mpsc::UnboundedSender<Route>,
    events: mpsc::UnboundedSender<GrantEvent>,
}

impl McpGuest {
    /// Make the fake guest. Call inside a `LocalSet`.
    /// Returns:
    ///   Client of the guest, the receiver of the routes and the receiver
    ///   of the grant events.
    pub fn start() -> (
        supervisor::Client,
        mpsc::UnboundedReceiver<Route>,
        mpsc::UnboundedReceiver<GrantEvent>,
    ) {
        let (routes, routes_rx) = mpsc::unbounded_channel();
        let (events, events_rx) = mpsc::unbounded_channel();
        let guest = capnp_rpc::new_client::<supervisor::Client, _>(Self { routes, events });
        (super::rpc_loopback(guest.client), routes_rx, events_rx)
    }
}

impl supervisor::Server for McpGuest {
    async fn add_mcp_server(
        self: Rc<Self>,
        params: supervisor::AddMcpServerParams,
        mut results: supervisor::AddMcpServerResults,
    ) -> Result<(), capnp::Error> {
        let params = params.get()?;
        let _ = self.routes.send(Route {
            session: params.get_session()?.to_str()?.to_string(),
            name: params.get_name()?.to_str()?.to_string(),
            host: params.get_host()?,
        });
        results
            .get()
            .set_grant(capnp_rpc::new_client(FakeGrant::new(self.events.clone())));
        Ok(())
    }
}

/// Grant that reports its release and its drop.
pub struct FakeGrant {
    events: mpsc::UnboundedSender<GrantEvent>,
}

impl FakeGrant {
    /// Make a grant that sends its release and its drop to `events`.
    pub fn new(events: mpsc::UnboundedSender<GrantEvent>) -> Self {
        Self { events }
    }
}

impl mcp_grant::Server for FakeGrant {
    async fn release(
        self: Rc<Self>,
        _: mcp_grant::ReleaseParams,
        _: mcp_grant::ReleaseResults,
    ) -> Result<(), capnp::Error> {
        let _ = self.events.send(GrantEvent::Released);
        Ok(())
    }
}

impl Drop for FakeGrant {
    fn drop(&mut self) {
        let _ = self.events.send(GrantEvent::Dropped);
    }
}
