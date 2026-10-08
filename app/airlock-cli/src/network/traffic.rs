//! Byte counting for each connection.
//!
//! Counts the bytes that each connection sends and receives, for the up/down
//! column of the Monitor tab. Counts TLS, plain and passthrough connections.

use std::cell::Cell;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::sync::broadcast;

use super::io::{BoxRead, BoxWrite, Transport};

/// Minimum time between two `Traffic` events for the same connection.
/// A busy relay moves chunks much faster than the TUI can draw. Without this
/// limit, too many events would fill the event channel, with no visible
/// benefit.
const EMIT_INTERVAL: Duration = Duration::from_millis(500);

/// Byte counts of one connection. Sends throttled `Traffic` events when
/// the counts change.
///
/// The counter is on the raw RPC stream of the guest connection, *below*
/// the TLS that the proxy terminates. Thus the counts are wire bytes,
/// including encrypted records and the handshake. A packet capture on the
/// guest interface would show the same counts.
pub struct TrafficCounter {
    id: u64,
    up: Cell<u64>,
    down: Cell<u64>,
    /// Totals of the last sent event. Used to send no event when the totals
    /// did not change.
    sent: Cell<(u64, u64)>,
    last_emit: Cell<Instant>,
    events: broadcast::Sender<airlock_monitor::NetworkEvent>,
}

impl TrafficCounter {
    /// Make a counter for connection `id`.
    /// Returns:
    ///   The counter, or `None` if the event channel has no subscribers.
    ///   With `None`, the caller does not wrap the transport. Then there is
    ///   no work for each byte and no extra layer. Runs without the monitor
    ///   do only one `receiver_count()` check when the connection starts.
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

    /// Send an event if the throttle window is over and the totals changed.
    fn maybe_emit(&self) {
        if self.last_emit.get().elapsed() >= EMIT_INTERVAL {
            self.emit();
        }
    }

    /// Send the current totals now, if they are different from the last
    /// sent totals. Call it when the connection closes. Then a short
    /// transfer that ends before the throttle window also sends a report.
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

/// Wrap a duplex container-side stream to count the bytes that go through
/// it. Use it on the TLS path, before the TLS layer is added on top. There
/// the counter must be *below* the TLS layer, and the stream is not yet
/// split into halves.
pub fn count_stream<S>(inner: S, counter: &Rc<TrafficCounter>) -> CountingStream<S> {
    CountingStream {
        inner,
        counter: counter.clone(),
    }
}

/// Duplex stream that counts bytes. The duplex version of [`CountingRead`]
/// and [`CountingWrite`]. It is generic, not boxed, so the TLS handshake
/// keeps its concrete stream type.
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

/// Wrap a container-side transport to count the bytes that go through it.
/// Reads from the container are "up" (guest to server). Writes to the
/// container are "down" (server to guest).
///
/// Use it only where the transport *is* the raw stream: the plain HTTP and
/// passthrough paths. The TLS path uses [`count_stream`]. If not, it counts
/// decrypted payload, not wire bytes.
/// Args:
///  - `t`: Container-side transport
///  - `counter`: Counter, or `None` for no counting.
///
/// Returns:
///   The wrapped transport, or `t` with no change if `counter` is `None`.
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

/// Read half that counts the bytes read ("up").
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

/// Write half that counts the bytes written ("down").
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

    // Forward this call, and do not use the default. Then a vectored writer
    // below keeps its fast path. The h2 client writes frame headers and
    // payloads as separate slices.
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
