//! Interface between the network proxy and the network services.
//!
//! A service uses this interface to handle the requests to the hosts that it
//! owns. The network does not know what a service does with a request (token
//! swaps, sign-in, backstops). That is the concern of the services.

use futures::future::LocalBoxFuture;
use hyper::{Request, Response};

use super::http::ResponseBody;
use super::target::{Endpoint, InjectedSecret, NetworkTarget};

/// Function that sends one request upstream: the remaining part of the
/// relay.
pub type Next = Box<
    dyn FnOnce(
        Request<ResponseBody>,
    ) -> LocalBoxFuture<'static, anyhow::Result<Response<ResponseBody>>>,
>;

/// Proxy of one provider for the hosts that it owns.
///
/// The network layer matches the target of each connection against the
/// [`targets`](Interceptor::targets) of all interceptors. On a match, it
/// gives each request on a TLS connection to [`send`](Interceptor::send),
/// instead of forwarding the request with no change.
pub trait Interceptor {
    /// Display name for logging and diagnostics.
    fn name(&self) -> &str;

    /// Get the hosts that this interceptor owns. The network allows them
    /// (except under `deny-always` or a deny rule), intercepts their TLS
    /// connections, and never uses passthrough for them.
    fn targets(&self) -> &[NetworkTarget];

    /// Handle one request to an owned host, after Lua middleware.
    ///
    /// The implementation must set the request authority to `to`, replace
    /// surrogates with the real tokens and call `next`. Or it can send a
    /// local response. Responses must get to the guest with surrogates
    /// only.
    /// Args:
    ///  - `to`: Host and port that the guest connected to
    ///  - `req`: Request from the guest
    ///  - `injected`: Masked secrets that the inject rules put into the
    ///    request (already unmasked). These are the only credentials other
    ///    than surrogates that an API request can contain.
    ///  - `next`: Function that sends the request upstream.
    ///
    /// Returns:
    ///   Response for the guest, or error.
    fn send<'a>(
        &'a self,
        to: &'a Endpoint,
        req: Request<ResponseBody>,
        injected: &'a [InjectedSecret],
        next: Next,
    ) -> LocalBoxFuture<'a, anyhow::Result<Response<ResponseBody>>>;
}
