//! Trait abstraction over hyper's HTTP/1.1 and HTTP/2 client senders so
//! the middleware layer doesn't need to know which protocol is in use.

use std::cell::RefCell;
use std::pin::Pin;

use hyper::body::Incoming;
use hyper::{Request, Response};

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
        req: Request<ResponseBody>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>> {
        Box::pin(self.0.borrow_mut().send_request(req))
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
