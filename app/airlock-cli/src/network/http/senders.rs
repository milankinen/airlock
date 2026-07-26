//! Trait abstraction over hyper's HTTP/1.1 and HTTP/2 client senders so
//! the middleware layer doesn't need to know which protocol is in use.

use std::cell::RefCell;
use std::pin::Pin;

use hyper::body::Incoming;
use hyper::header::{HOST, HeaderValue};
use hyper::{Request, Response, Uri};

use crate::network::http::ResponseBody;

/// Send an HTTP request over either h1 or h2.
pub trait RequestSender {
    fn send(
        &self,
        req: Request<ResponseBody>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>>;
}

/// HTTP/1.1 sender wrapper. Uses `RefCell` because h1 `SendRequest` requires `&mut`.
pub struct H1Sender(pub RefCell<hyper::client::conn::http1::SendRequest<ResponseBody>>);
impl RequestSender for H1Sender {
    fn send(
        &self,
        mut req: Request<ResponseBody>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>> {
        to_origin_form(&mut req);
        Box::pin(self.0.borrow_mut().send_request(req))
    }
}

/// Rewrite an absolute-form request into the origin-form + `Host` pair that
/// HTTP/1.1 origin servers expect.
///
/// A request that reached us over h2 carries its authority in the URI (built
/// from `:authority`) and has no `Host` header at all, since h2 has none.
/// Forwarded verbatim that becomes `GET https://host/path HTTP/1.1` with no
/// `Host`, which strict servers — nginx among them — answer with 400. This
/// happens whenever the two hops disagree on protocol: an h2 container
/// talking to an http/1.1 upstream, or h2-with-prior-knowledge over
/// cleartext (where the upstream is never h2).
///
/// Requests that arrived over h1 are already in origin form and carry their
/// own `Host`, so this is a no-op for them.
fn to_origin_form(req: &mut Request<ResponseBody>) {
    let Some(authority) = req.uri().authority().cloned() else {
        return;
    };
    if !req.headers().contains_key(HOST)
        && let Ok(value) = HeaderValue::from_str(authority.as_str())
    {
        req.headers_mut().insert(HOST, value);
    }
    let path = req
        .uri()
        .path_and_query()
        .map_or_else(|| "/".to_string(), ToString::to_string);
    if let Ok(uri) = path.parse::<Uri>() {
        *req.uri_mut() = uri;
    }
}

/// HTTP/2 sender wrapper. h2 `SendRequest` is clone-friendly, no `RefCell` needed.
pub struct H2Sender(pub hyper::client::conn::http2::SendRequest<ResponseBody>);
impl RequestSender for H2Sender {
    fn send(
        &self,
        req: Request<ResponseBody>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>> {
        let mut sender = self.0.clone();
        Box::pin(async move { sender.send_request(req).await })
    }
}
