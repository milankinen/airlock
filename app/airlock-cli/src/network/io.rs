//! I/O primitives for bridging RPC byte streams with tokio async I/O.
//!
//! The network proxy needs to treat both real TCP sockets and Cap'n Proto
//! RPC channels as `AsyncRead + AsyncWrite`. This module provides the
//! adapters that make that possible.

use std::cell::RefCell;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};

use airlock_common::network_capnp::tcp_sink;
use bytes::{Buf, Bytes};
use capnp::capability::Promise;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::mpsc;

/// Boxed read half for type-erased async streams.
pub type BoxRead = Box<dyn AsyncRead + Unpin>;
/// Boxed write half for type-erased async streams.
pub type BoxWrite = Box<dyn AsyncWrite + Unpin>;

/// A connection endpoint with boxed read/write streams and h2 flag.
pub struct Transport {
    pub read: BoxRead,
    pub write: BoxWrite,
    pub h2: bool,
}

impl Transport {
    /// A black-hole transport used as the "server" side when policy denies
    /// the connection: reads return EOF, writes are discarded. Paired with
    /// [`super::tcp::relay`] it causes the relay to tear the connection
    /// down immediately; paired with [`super::http::relay`] it short-
    /// circuits at the `!target.allowed` branch before the sender is used.
    pub fn null() -> Self {
        Self {
            read: Box::new(tokio::io::empty()),
            write: Box::new(tokio::io::sink()),
            h2: false,
        }
    }
}

/// Prepend buffered bytes to an `AsyncRead` stream.
pub struct PrefixedRead {
    prefix: Bytes,
    inner: BoxRead,
}

impl PrefixedRead {
    pub fn new(prefix: Bytes, inner: BoxRead) -> Self {
        Self { prefix, inner }
    }
}

impl AsyncRead for PrefixedRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            let n = self.prefix.len().min(buf.remaining());
            buf.put_slice(&self.prefix[..n]);
            self.prefix.advance(n);
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut *self.inner).poll_read(cx, buf)
    }
}

/// Bridges an mpsc channel + RPC sink into `AsyncRead + AsyncWrite`.
///
/// `send` on the sink is a capnp `-> stream` method: over a real (two-party)
/// connection it writes to the wire immediately but only *resolves* once the
/// per-stream flow-control window has room again (or the stream failed).
/// `write_ack` holds that promise between polls so the write side honors it
/// instead of racing ahead.
pub struct RpcTransport {
    prefix: Bytes,
    rx: mpsc::Receiver<Bytes>,
    client_sink: tcp_sink::Client,
    pending: Bytes,
    /// Ack for the last `send` or `close`; shutdown is terminal, so they
    /// never overlap.
    write_ack: Option<Promise<(), capnp::Error>>,
    /// Set once `close` has been requested, so a repeated `poll_shutdown`
    /// after the close resolves doesn't resend it.
    close_sent: bool,
}

impl RpcTransport {
    /// Create a transport with an optional prefix (pre-read bytes), an mpsc
    /// receiver for incoming data, and an RPC sink for outgoing data.
    pub fn new(
        prefix: impl Into<Bytes>,
        rx: mpsc::Receiver<Bytes>,
        client_sink: tcp_sink::Client,
    ) -> Self {
        Self {
            prefix: prefix.into(),
            rx,
            client_sink,
            pending: Bytes::new(),
            write_ack: None,
            close_sent: false,
        }
    }

    /// Drive `write_ack` to completion, translating a flow-control failure
    /// (e.g. the guest sink is gone) into an I/O error. `Ready(Ok(()))`
    /// means there is nothing outstanding to wait for.
    fn poll_ack(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let Some(promise) = self.write_ack.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        match Pin::new(promise).poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(result) => {
                self.write_ack = None;
                Poll::Ready(
                    result.map_err(|e| std::io::Error::new(std::io::ErrorKind::BrokenPipe, e)),
                )
            }
        }
    }

    fn drain(src: &mut Bytes, buf: &mut ReadBuf<'_>) {
        let n = src.len().min(buf.remaining());
        buf.put_slice(&src[..n]);
        src.advance(n);
    }
}

impl AsyncRead for RpcTransport {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.prefix.is_empty() {
            Self::drain(&mut self.prefix, buf);
            return Poll::Ready(Ok(()));
        }
        if !self.pending.is_empty() {
            Self::drain(&mut self.pending, buf);
            return Poll::Ready(Ok(()));
        }
        match self.rx.poll_recv(cx) {
            Poll::Ready(Some(mut data)) => {
                Self::drain(&mut data, buf);
                if !data.is_empty() {
                    self.pending = data;
                }
                Poll::Ready(Ok(()))
            }
            Poll::Ready(None) => Poll::Ready(Ok(())),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for RpcTransport {
    /// Waits for the previous `send`'s ack before issuing the next one.
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        match self.poll_ack(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }
        let mut req = self.client_sink.send_request();
        req.get().set_data(buf);
        self.write_ack = Some(req.send());
        Poll::Ready(Ok(buf.len()))
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.poll_ack(cx)
    }

    /// Flushes the pending `send` ack, then issues `close` once and waits
    /// for it the same way.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }
        if !self.close_sent {
            self.close_sent = true;
            // `close` returns a response; drop it to fit `write_ack`.
            let response = self.client_sink.close_request().send().promise;
            self.write_ack = Some(Promise::from_future(async move {
                response.await?;
                Ok(())
            }));
        }
        self.poll_ack(cx)
    }
}

/// Shared error state between relay task and ChannelSink.
pub type RelayError = Rc<RefCell<Option<String>>>;

/// RPC interface for the supervisor to push container bytes into the channel.
pub struct ChannelSink {
    tx: RefCell<Option<mpsc::Sender<Bytes>>>,
    error: RelayError,
}

impl ChannelSink {
    /// Create a new sink with the given channel and shared error state.
    pub fn new(tx: mpsc::Sender<Bytes>, error: RelayError) -> Self {
        Self {
            tx: RefCell::new(Some(tx)),
            error,
        }
    }
}

impl tcp_sink::Server for ChannelSink {
    async fn send(self: Rc<Self>, params: tcp_sink::SendParams) -> Result<(), capnp::Error> {
        if let Some(err) = self.error.borrow().as_ref() {
            return Err(capnp::Error::failed(err.clone()));
        }
        let data = params.get()?.get_data()?;
        let tx = self.tx.borrow().clone();
        match tx.as_ref() {
            Some(tx) => {
                tx.send(Bytes::copy_from_slice(data)).await.map_err(|_| {
                    let err = self.error.borrow();
                    let msg = err.as_deref().unwrap_or("relay closed");
                    capnp::Error::failed(msg.to_string())
                })?;
            }
            None => {
                return Err(capnp::Error::failed("channel closed".to_string()));
            }
        }
        Ok(())
    }

    async fn close(
        self: Rc<Self>,
        _params: tcp_sink::CloseParams,
        _results: tcp_sink::CloseResults,
    ) -> Result<(), capnp::Error> {
        self.tx.borrow_mut().take();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Wake, Waker};

    use tokio::sync::Notify;

    use super::*;

    /// A `tcp_sink::Server` whose `send` blocks on a shared gate, standing
    /// in for a guest that has stopped reading. A local (non-networked)
    /// capability skips the two-party flow-control window, but its
    /// streaming dispatch is lazy — `send_request().send()` only actually
    /// calls into `send` the first time the returned promise is polled —
    /// so holding the gate shut keeps that first poll, and so `write_ack`,
    /// pending.
    #[derive(Clone)]
    struct GatedSink {
        gate: Arc<Notify>,
    }

    impl tcp_sink::Server for GatedSink {
        async fn send(self: Rc<Self>, _params: tcp_sink::SendParams) -> Result<(), capnp::Error> {
            self.gate.notified().await;
            Ok(())
        }

        async fn close(
            self: Rc<Self>,
            _params: tcp_sink::CloseParams,
            _results: tcp_sink::CloseResults,
        ) -> Result<(), capnp::Error> {
            Ok(())
        }
    }

    /// A `Waker` that records whether it was ever woken, so a test can
    /// assert a poll's waker was actually invoked rather than merely
    /// re-polling until something resolves.
    struct FlagWake(Arc<AtomicBool>);

    impl Wake for FlagWake {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    #[test]
    fn poll_write_blocks_until_previous_send_acks_then_wakes() {
        let (_tx, rx) = mpsc::channel::<Bytes>(1);
        let gate = Arc::new(Notify::new());
        let client_sink: tcp_sink::Client = capnp_rpc::new_client(GatedSink { gate: gate.clone() });
        let mut transport = RpcTransport::new(Bytes::new(), rx, client_sink);

        let woken = Arc::new(AtomicBool::new(false));
        let waker = Waker::from(Arc::new(FlagWake(woken.clone())));
        let mut cx = Context::from_waker(&waker);

        // The first write completes immediately: nothing is outstanding
        // yet, so there's nothing to wait for.
        match Pin::new(&mut transport).poll_write(&mut cx, b"first") {
            Poll::Ready(Ok(5)) => {}
            other => panic!("expected first write to complete immediately, got {other:?}"),
        }

        // The gate is still shut, so a second write must block on the
        // first's ack rather than queuing more data behind an unbounded
        // backlog.
        match Pin::new(&mut transport).poll_write(&mut cx, b"second") {
            Poll::Pending => {}
            Poll::Ready(other) => {
                panic!("expected second write to block on backpressure, got {other:?}")
            }
        }
        assert!(
            !woken.load(Ordering::SeqCst),
            "should not be woken before the gate opens"
        );

        // Releasing the gate lets `send` return, which must wake the writer
        // that was waiting on its ack.
        gate.notify_one();
        assert!(
            woken.load(Ordering::SeqCst),
            "should be woken once the ack resolves"
        );

        match Pin::new(&mut transport).poll_write(&mut cx, b"second") {
            Poll::Ready(Ok(6)) => {}
            other => panic!("expected the retried write to complete, got {other:?}"),
        }
    }
}
