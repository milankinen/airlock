//! The interceptor seam between the network proxy and the services behind
//! it ([`crate::services`]).
//!
//! An [`Interceptor`] is one provider's proxy over the hosts it owns: the
//! network layer resolves a connection's target, matches it against the
//! interceptors' [`targets`](Interceptor::targets), and on a match hands
//! every request on the connection to [`send`](Interceptor::send) around
//! the upstream send, instead of forwarding it as is. The network knows
//! nothing about what an interceptor does with a request (token swaps,
//! sign-in, backstops) — that is the services' concern.

use futures::future::LocalBoxFuture;
use hyper::{Request, Response};

use super::http::ResponseBody;
use super::target::{Endpoint, InjectedSecret, NetworkTarget};

/// The upstream send of one request: the rest of the relay.
pub type Next = Box<
    dyn FnOnce(
        Request<ResponseBody>,
    ) -> LocalBoxFuture<'static, anyhow::Result<Response<ResponseBody>>>,
>;

/// One provider's proxy over the hosts it owns.
pub trait Interceptor {
    /// Display name for logging and diagnostics.
    fn name(&self) -> &str;

    /// The hosts this interceptor owns: allowed (unless `deny-always` or a
    /// deny rule), always intercepted, never passthrough.
    fn targets(&self) -> &[NetworkTarget];

    /// Handle one request to an owned host `to` (the host and port the
    /// guest connected to), after Lua middleware: pin the request's
    /// authority to `to`, swap surrogates for the real tokens and call
    /// `next` (the upstream), or answer locally. `injected` are the masked
    /// secrets the inject rules put into the request (already unmasked):
    /// the only credentials besides surrogates that an API request may
    /// carry. Responses reach the guest with surrogates only.
    fn send<'a>(
        &'a self,
        to: &'a Endpoint,
        req: Request<ResponseBody>,
        injected: &'a [InjectedSecret],
        next: Next,
    ) -> LocalBoxFuture<'a, anyhow::Result<Response<ResponseBody>>>;
}
