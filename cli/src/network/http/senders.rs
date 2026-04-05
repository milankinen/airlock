use std::cell::RefCell;
use std::pin::Pin;

use hyper::body::Incoming;
use hyper::{Request, Response};

use crate::network::http::ResponseBody;

/// Abstraction over h1/h2 client senders.
pub trait RequestSender {
    fn send(
        &self,
        req: Request<ResponseBody>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>>;
}

pub struct H1Sender(pub RefCell<hyper::client::conn::http1::SendRequest<ResponseBody>>);
impl RequestSender for H1Sender {
    fn send(
        &self,
        req: Request<ResponseBody>,
    ) -> Pin<Box<dyn Future<Output = Result<Response<Incoming>, hyper::Error>>>> {
        Box::pin(self.0.borrow_mut().send_request(req))
    }
}

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
