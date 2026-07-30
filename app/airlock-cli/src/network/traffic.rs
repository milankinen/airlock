//! Per-connection byte accounting for the Monitor tab's up/down column.
//!
//! Counting attaches to the raw RPC stream carrying the guest's
//! connection, *underneath* any TLS the proxy terminates. The figures are
//! therefore wire bytes: encrypted records and handshake included, which
//! is what a packet capture on the guest's interface would show.
//!
//! Two entry points, because the TLS and plain paths reach this at
//! different points in the stack. [`count_stream`] wraps a duplex stream
//! before the TLS layer is built on top of it; [`count`] wraps an already
//! split [`Transport`] on the plain and passthrough paths, where the raw
//! stream *is* the transport.
//!
//! The whole thing is opt-in per connection: [`TrafficCounter::new`]
//! returns `None` when nothing is subscribed to the event channel, and
//! both wrappers are then skipped entirely — no per-byte work and no
//! extra layer in the stack. Non-monitor runs pay one `receiver_count()`
//! check at connection setup and nothing else.

use std::cell::Cell;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::broadcast;

use super::io::{BoxRead, BoxWrite, Transport};

/// Minimum gap between two `Traffic` events for the same connection.
/// A busy relay moves chunks far faster than the TUI can redraw, so
/// without this the event channel would be swamped for no visible gain.
const EMIT_INTERVAL: Duration = Duration::from_millis(500);

/// Accumulates byte counts for one connection and emits throttled
/// `Traffic` events as they change.
pub struct TrafficCounter {
    id: u64,
    up: Cell<u64>,
    down: Cell<u64>,
    /// Totals as of the last emitted event — used to skip emitting when
    /// nothing moved since.
    sent: Cell<(u64, u64)>,
    last_emit: Cell<Instant>,
    events: broadcast::Sender<airlock_monitor::NetworkEvent>,
}

impl TrafficCounter {
    /// Create a counter for connection `id`, or `None` when no one is
    /// listening — the caller then skips wrapping the transport entirely.
    pub fn new(
        id: u64,
        events: &broadcast::Sender<airlock_monitor::NetworkEvent>,
    ) -> Option<Rc<Self>> {
        if events.receiver_count() == 0 {
            return None;
        }
        Some(Rc::new(Self {
            id,
            up: Cell::new(0),
            down: Cell::new(0),
            sent: Cell::new((0, 0)),
            last_emit: Cell::new(Instant::now()),
            events: events.clone(),
        }))
    }

    fn add_up(&self, n: u64) {
        self.up.set(self.up.get() + n);
        self.maybe_emit();
    }

    fn add_down(&self, n: u64) {
        self.down.set(self.down.get() + n);
        self.maybe_emit();
    }

    /// Emit if the throttle window has elapsed and the totals moved.
    fn maybe_emit(&self) {
        if self.last_emit.get().elapsed() >= EMIT_INTERVAL {
            self.emit();
        }
    }

    /// Emit the current totals unconditionally, as long as they differ
    /// from what was last sent. Called on connection close so a short
    /// transfer that never outlived the throttle window still reports.
    pub fn flush(&self) {
        self.emit();
    }

    fn emit(&self) {
        let (up, down) = (self.up.get(), self.down.get());
        if self.sent.get() == (up, down) {
            return;
        }
        self.sent.set((up, down));
        self.last_emit.set(Instant::now());
        let info = airlock_monitor::TrafficInfo {
            id: self.id,
            up,
            down,
        };
        let _ = self
            .events
            .send(airlock_monitor::NetworkEvent::Traffic(std::sync::Arc::new(
                info,
            )));
    }
}

/// Wrap a duplex container-side stream so bytes crossing it are counted.
/// Used on the TLS path, where counting has to sit *below* the TLS layer
/// and the stream hasn't been split into halves yet.
pub fn count_stream<S>(inner: S, counter: &Rc<TrafficCounter>) -> CountingStream<S> {
    CountingStream {
        inner,
        counter: counter.clone(),
    }
}

/// Duplex counterpart to [`CountingRead`] / [`CountingWrite`]. Generic
/// rather than boxed so the TLS handshake keeps its concrete stream type.
pub struct CountingStream<S> {
    inner: S,
    counter: Rc<TrafficCounter>,
}

impl<S: AsyncRead + Unpin> AsyncRead for CountingStream<S> {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = Pin::new(&mut self.inner).poll_read(cx, buf);
        if res.is_ready() {
            let n = buf.filled().len().saturating_sub(before);
            if n > 0 {
                self.counter.add_up(n as u64);
            }
        }
        res
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CountingStream<S> {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let res = Pin::new(&mut self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = res {
            self.counter.add_down(n as u64);
        }
        res
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let res = Pin::new(&mut self.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = res {
            self.counter.add_down(n as u64);
        }
        res
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// Wrap a container-side transport so bytes crossing it are counted.
/// Reads from the container are "up" (guest → server); writes to it are
/// "down" (server → guest). A `None` counter returns `t` unchanged.
///
/// Only correct where the transport *is* the raw stream — the plain-HTTP
/// and passthrough paths. The TLS path uses [`count_stream`] instead, or
/// it would count decrypted payload rather than wire bytes.
pub fn count(t: Transport, counter: Option<&Rc<TrafficCounter>>) -> Transport {
    let Some(counter) = counter else {
        return t;
    };
    Transport {
        read: Box::new(CountingRead {
            inner: t.read,
            counter: counter.clone(),
        }),
        write: Box::new(CountingWrite {
            inner: t.write,
            counter: counter.clone(),
        }),
        h2: t.h2,
    }
}

struct CountingRead {
    inner: BoxRead,
    counter: Rc<TrafficCounter>,
}

impl AsyncRead for CountingRead {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let before = buf.filled().len();
        let res = Pin::new(&mut *self.inner).poll_read(cx, buf);
        if res.is_ready() {
            let n = buf.filled().len().saturating_sub(before);
            if n > 0 {
                self.counter.add_up(n as u64);
            }
        }
        res
    }
}

struct CountingWrite {
    inner: BoxWrite,
    counter: Rc<TrafficCounter>,
}

impl AsyncWrite for CountingWrite {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let res = Pin::new(&mut *self.inner).poll_write(cx, buf);
        if let Poll::Ready(Ok(n)) = res {
            self.counter.add_down(n as u64);
        }
        res
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut *self.inner).poll_shutdown(cx)
    }

    // Forwarded rather than left to the default so a vectored writer
    // underneath keeps its fast path — the h2 client writes frame
    // headers and payloads as separate slices.
    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let res = Pin::new(&mut *self.inner).poll_write_vectored(cx, bufs);
        if let Poll::Ready(Ok(n)) = res {
            self.counter.add_down(n as u64);
        }
        res
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}
