//! HTTP/1.1 upgrade support, for example WebSocket and `CONNECT`.
//!
//! Follows the upgrade state of one guest connection when both sides use
//! HTTP/1.1. After the upgrade, the proxy relays raw bytes between the sides.

use std::cell::Cell;

use hyper::body::Bytes;
use hyper::header::{CONNECTION, HeaderValue, UPGRADE};
use hyper::{Method, Request, Response, StatusCode, Version};

use super::{HyperIo, ResponseBody, text_response};
use crate::network::io;

/// Upgrade state of one guest connection.
#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum State {
    /// No upgrade request.
    #[default]
    None,
    /// An upgrade request was forwarded. The upstream reply is pending.
    Requested,
    /// The upstream accepted the switch.
    Switched,
}

/// Upgrade progress of one guest connection. The service (which sees
/// requests and replies) and the connection driver share it.
///
/// The upgrade support of hyper (`with_upgrades`, `hyper::upgrade::on`)
/// needs `Send` IO, and the RPC guest transport is not `Send`. Thus the
/// relay lets both hyper connections finish on the switch. Neither one closes
/// its socket then. The relay gets the sockets back with `into_parts`
/// and relays the bytes with no change.
#[derive(Default)]
pub struct Upgrade(Cell<State>);

impl Upgrade {
    /// Return true if `req` is an upgrade request. This is the same test as
    /// in hyper (`Server::parse`): an `Upgrade` header on HTTP/1.1 only, or
    /// else `CONNECT`. hyper ends the guest connection with
    /// `Dispatched::Upgrade` for exactly these requests, so the relay must
    /// use the same test.
    pub fn wants<B>(req: &Request<B>) -> bool {
        if req.headers().contains_key(UPGRADE) {
            req.version() == Version::HTTP_11
        } else {
            req.method() == Method::CONNECT
        }
    }

    /// Record an upgrade request from the guest. Call it before middleware
    /// and the send. The upstream h1 connection ends immediately when a 101
    /// arrives, possibly before the service sees the reply. A script can
    /// still deny the request.
    pub fn requested(&self) {
        self.0.set(State::Requested);
    }

    /// Record the reply of the upstream, before middleware.
    pub fn upstream_replied<B>(&self, method: &Method, resp: &Response<B>) {
        if switches(method, resp.status()) {
            self.0.set(State::Switched);
        }
    }

    /// Prepare the reply to the guest, after middleware.
    ///
    /// If the upstream switched and the guest sees the switch, the reply
    /// does not change. In all other cases, the guest connection ends with
    /// this reply (`Connection: close`):
    ///  * The upstream may already be gone. The shutdown that usually tells
    ///    the guest about it is disabled while an upgrade is in progress.
    ///    Thus a close here is the one uniform answer. This also covers an
    ///    upgrade request that a script denied before the send.
    ///  * A script can set a switch status when the upstream did not switch.
    ///    The reply then becomes a 502, because there is no upstream stream
    ///    to relay.
    ///  * A script can change an accepted switch into a different reply.
    ///    The upstream socket is then dropped.
    pub fn reply(&self, method: &Method, resp: &mut Response<ResponseBody>) {
        match (self.switched(), switches(method, resp.status())) {
            (true, true) => return,
            (true, false) => self.0.set(State::Requested),
            (false, true) => {
                *resp = text_response(
                    StatusCode::BAD_GATEWAY,
                    "upgrade not accepted by upstream\n",
                );
            }
            (false, false) => {}
        }
        resp.headers_mut()
            .insert(CONNECTION, HeaderValue::from_static("close"));
    }

    /// Return true if the guest sent an upgrade request on this connection.
    pub fn in_flight(&self) -> bool {
        self.0.get() != State::None
    }

    /// Return true if the upstream accepted the switch.
    pub fn switched(&self) -> bool {
        self.0.get() == State::Switched
    }
}

/// Return true if the reply switches the connection. This is the same
/// client-side test as in hyper (`Client::decoder`): 101 to all requests, or
/// a 2xx other than 204 to `CONNECT`.
fn switches(method: &Method, status: StatusCode) -> bool {
    status == StatusCode::SWITCHING_PROTOCOLS
        || (*method == Method::CONNECT && status.is_success() && status != StatusCode::NO_CONTENT)
}

/// Make a relay side from a hyper IO object.
/// Args:
///  - `hyper_io`: IO object of the finished hyper connection
///  - `read_buf`: Bytes that hyper already read after the end of the HTTP
///    exchange.
///
/// Returns:
///   Transport that first returns `read_buf`, then reads from the IO.
pub fn transport(hyper_io: HyperIo, read_buf: Bytes) -> io::Transport {
    let (read, write) = hyper_io.into_inner().into_inner();
    io::Transport {
        read: Box::new(io::PrefixedRead::new(read_buf, read)),
        write,
        h2: false,
    }
}
