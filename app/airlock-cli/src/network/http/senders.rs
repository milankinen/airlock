//! Upstream HTTP request senders.
//!
//! Gives one interface to send requests over HTTP/1.1 and HTTP/2. Thus the
//! middleware layer does not need to know which protocol the upstream uses.

use std::cell::RefCell;
use std::pin::Pin;

use hyper::body::Incoming;
use hyper::header::{HOST, HeaderValue};
use hyper::{Method, Request, Response, Uri};

use crate::network::http::ResponseBody;

/// Sends an HTTP request on h1 or h2.
pub trait RequestSender {
    /// Send `req` to the upstream.
    /// Returns:
    ///   Future that gives the upstream response.
    fn send(
        &self,
        req: Request<ResponseBody>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>>;
}

/// HTTP/1.1 sender. Uses `RefCell` because h1 `SendRequest` needs `&mut`.
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

/// Change an absolute-form request into the origin form and `Host` header
/// that HTTP/1.1 origin servers expect.
///
/// A request that came on h2 has its authority in the URI (from
/// `:authority`) and has no `Host` header, because h2 has no `Host`. With
/// no change, it becomes `GET https://host/path HTTP/1.1` with no `Host`.
/// Strict servers (for example nginx) send 400 for it. This occurs when the
/// two sides use different protocols: an h2 container with an http/1.1
/// upstream, or h2 with prior knowledge on cleartext (where the upstream is
/// never h2).
///
/// Requests that came on h1 are already in origin form and have their own
/// `Host`. For them, this function changes nothing.
fn to_origin_form(req: &mut Request<ResponseBody>) {
    // `CONNECT host:port` is always in authority form.
    if req.method() == Method::CONNECT {
        return;
    }
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

/// HTTP/2 sender. h2 `SendRequest` is cheap to clone, so no `RefCell` is
/// necessary.
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
