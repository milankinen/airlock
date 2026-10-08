//! I/O adapters for the network proxy.
//!
//! Gives the proxy one common type for all connection endpoints: TCP sockets,
//! streams to and from the guest, and streams with bytes that the proxy has
//! already read and must put back.

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

/// Connection endpoint with boxed read and write streams.
pub struct Transport {
    /// Read half.
    pub read: BoxRead,
    /// Write half.
    pub write: BoxWrite,
    /// True if the endpoint uses HTTP/2.
    pub h2: bool,
}

impl Transport {
    /// Make an empty transport for the server side of a denied connection.
    /// Reads return EOF and writes are discarded.
    ///
    /// With [`super::tcp::relay`], the relay closes the connection
    /// immediately. With [`super::http::relay`], the relay stops at the
    /// `!target.allowed` branch before it uses the sender.
    pub fn null() -> Self {
        Self {
            read: Box::new(tokio::io::empty()),
            write: Box::new(tokio::io::sink()),
            h2: false,
        }
    }
}

/// `AsyncRead` stream that first returns buffered bytes and then reads from
/// the inner stream.
pub struct PrefixedRead {
    prefix: Bytes,
    inner: BoxRead,
}

impl PrefixedRead {
    /// Make a stream that returns `prefix` before the data of `inner`.
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

/// `AsyncRead + AsyncWrite` adapter for an mpsc channel (read side) and an
/// RPC sink (write side).
pub struct RpcTransport {
    prefix: Bytes,
    rx: mpsc::Receiver<Bytes>,
    client_sink: tcp_sink::Client,
    pending: Bytes,
    /// Ack for the last `send` or `close`. Shutdown is the last operation,
    /// so the two never overlap.
    ///
    /// `send` on the sink is a capnp `-> stream` method. On a real
    /// (two-party) connection, it writes to the wire immediately. But it
    /// *resolves* only when the flow-control window of the stream has space
    /// again (or the stream failed). This field keeps that promise between
    /// polls, so the write side waits for it.
    write_ack: Option<Promise<(), capnp::Error>>,
    /// True after the `close` request. Thus a new `poll_shutdown` call after
    /// the close resolves does not send it again.
    close_sent: bool,
}

impl RpcTransport {
    /// Make an RPC transport.
    /// Args:
    ///  - `prefix`: Bytes that were already read. Can be empty.
    ///  - `rx`: Receiver for incoming data
    ///  - `client_sink`: RPC sink for outgoing data.
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

    /// Poll `write_ack` until it completes.
    /// Returns:
    ///   `Ready(Ok(()))` if no ack is pending. An I/O error if flow control
    ///   failed (for example, the guest sink is gone).
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
    /// Wait for the ack of the previous `send` before the next `send`.
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

    /// Wait for the pending `send` ack. Then send `close` one time and wait
    /// for its ack the same way.
    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        match self.as_mut().poll_flush(cx) {
            Poll::Pending => return Poll::Pending,
            Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
            Poll::Ready(Ok(())) => {}
        }
        if !self.close_sent {
            self.close_sent = true;
            // `close` returns a response. Drop it to fit the type of
            // `write_ack`.
            let response = self.client_sink.close_request().send().promise;
            self.write_ack = Some(Promise::from_future(async move {
                response.await?;
                Ok(())
            }));
        }
        self.poll_ack(cx)
    }
}

/// Error state that the relay task and [`ChannelSink`] share.
pub type RelayError = Rc<RefCell<Option<String>>>;

/// RPC sink that the supervisor uses to send container bytes into the
/// channel.
pub struct ChannelSink {
    tx: RefCell<Option<mpsc::Sender<Bytes>>>,
    error: RelayError,
}

impl ChannelSink {
    /// Make a sink with the given channel and shared error state.
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
    //! Tests for the flow control of the RPC transport.

    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::task::{Wake, Waker};

    use tokio::sync::Notify;

    use super::*;

    /// A guest sink whose `send` waits on a shared gate. It acts as a guest
    /// that does not read. A local capability has no flow-control window.
    /// But it calls `send` only when the returned promise is first polled.
    /// Thus a closed gate keeps that poll, and the write ack, pending.
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

    /// A waker that records if something woke it. A test can then check
    /// that the waker was called, not only that a new poll completes.
    struct FlagWake(Arc<AtomicBool>);

    impl Wake for FlagWake {
        fn wake(self: Arc<Self>) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// Test that a write waits for the ack of the previous send, and that
    /// the ack wakes the waiting writer. Without this, a guest that does not
    /// read makes the proxy buffer data with no limit.
    ///   1. Write once to a transport whose guest sink does not answer
    ///   2. Check that a second write is pending and the waker is not called
    ///   3. Open the gate and check that the waker is called
    ///   4. Write again and check that the write completes
    #[test]
    fn poll_write_with_unacked_send_blocks_until_ack_wakes_it() {
        let (_tx, rx) = mpsc::channel::<Bytes>(1);
        let gate = Arc::new(Notify::new());
        let client_sink: tcp_sink::Client = capnp_rpc::new_client(GatedSink { gate: gate.clone() });
        let mut transport = RpcTransport::new(Bytes::new(), rx, client_sink);

        let woken = Arc::new(AtomicBool::new(false));
        let waker = Waker::from(Arc::new(FlagWake(woken.clone())));
        let mut cx = Context::from_waker(&waker);
        match Pin::new(&mut transport).poll_write(&mut cx, b"first") {
            Poll::Ready(Ok(5)) => {}
            other => panic!("expected first write to complete immediately, got {other:?}"),
        }
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
        // No runtime runs here. The gate wakes the promise chain, and the
        // chain calls the test waker directly.
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
