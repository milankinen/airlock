# Monitor: transfer counters, response details, details scrolling

Five monitor changes. Two are TUI-local; two needed new data plumbed
from the host-side proxy; one is a keystroke that was leaking through.

## Leaving selection mode no longer double-fires the key

Selection mode (mouse capture released so the terminal can drag-select)
was exited by *any* keypress, and the key then fell through to its
normal handler. That's right for ordinary typing — the intent is "keep
working" — but wrong for the two keys the footer itself advertises:

- `Esc` ("exit") also dropped a literal `Esc` into the guest, so vim
  left insert mode, menus closed, and so on.
- `Ctrl+C` ("copy") also reached the guest as SIGINT, killing whatever
  was running. The copy is the *terminal's* doing — capture is off, so
  it owns the selection — which is precisely why forwarding the key on
  top of that is pure downside.

Both are now consumed by the mode switch; every other key still falls
through. `Ctrl+Shift+C` counts as well, since that's the copy binding on
GNOME Terminal and Konsole and crossterm reports the shifted char as
`'C'`. The same block also stopped being Sandbox-only, because the
details view can now enter selection mode too (below).

Worth noting for anyone extending this: whether these keys reach the
application at all is terminal-dependent, so the fix can only ensure we
don't *forward* them — it can't make a terminal that swallows `Ctrl+C`
for copy deliver it to us.

## Connections: `Transferred` column

New `NetworkEvent::Traffic { id, up, down }` carrying cumulative
counters, paired to a connection by the same id as `Connect`.

Counting happens in `network/traffic.rs`, which tallies bytes crossing
the container-side stream. The wrap sits *beneath* the TLS layer, so the
counters see wire bytes: encrypted records plus the handshake, matching
a capture on the guest's interface. `tls::detect` consumes the first
record before the stream is built, but those bytes are re-fed through it
as the prefix, so the ClientHello is counted rather than lost.

Getting under TLS costs a second entry point. The plain and passthrough
paths reach `traffic` holding a `Transport` whose halves already *are*
the raw stream, so `count()` wraps those. The TLS path has to attach
before `accept_container` builds the TLS layer, where the stream is
still duplex and unsplit — hence `count_stream()` and a `CountingStream`
that implements both halves. `accept_container` branches on the counter
and hands either the wrapped or the bare stream to a new generic
`handshake()`. Folding an `Option` into the wrapper instead would have
been less code, but it would put a live branch in every `poll_read` on
unwatched runs; branching once at setup keeps that path with no extra
layer at all.

Two properties worth keeping:

- **Nothing is wrapped when nobody is watching.** `TrafficCounter::new`
  returns `None` if the broadcast channel has no receivers, and `count()`
  then returns the transport untouched. Non-monitor runs — the common
  case — pay one `receiver_count()` check per connection and nothing per
  byte. This mirrors how `emit_connect`/`emit_request_event` already
  short-circuit.
- **`poll_write_vectored` is forwarded, not left to the default.** The
  default implementation collapses a vectored write into a single-slice
  one, which would have quietly de-optimized the h2 client's
  header+payload writes. Given the recent one-packet-per-wakeup
  throughput fix, silently regressing the relay's write path seemed
  worth avoiding.

Events are throttled to one per 500 ms per connection, with an
unconditional `flush()` when the connection closes — otherwise a
transfer shorter than the throttle window would report nothing at all.

`Traffic` for a connection already evicted by the 100-entry cap is
dropped, same as `Disconnect` already did.

The column costs 20 cells, which on an 80-column terminal would have
left ~11 for `Target` — every row an ellipsis. It's therefore dropped
whenever keeping it would push `Target` below 24 cells, which is the
first time a column in this panel has been responsive to width.

## Request details: response status and headers

`RequestInfo` gained an `id`, and a new `NetworkEvent::Response { id,
status, headers }` carries the reply back. Ids come from a plain
process-global counter in `http.rs` — they exist only to pair the two
halves within one TUI session, so nothing more elaborate is warranted.

All three response paths report: the normal upstream reply, the 403 on
the deny path, and the 502 the middleware error path synthesizes. The
502 in particular is worth surfacing — a request that failed inside the
proxy previously showed no response at all, which looked identical to
one still in flight.

It still looks identical to a request whose connection died before
replying. Distinguishing those would mean tracking request lifetime
against connection teardown; the view says `(no response yet)` and
leaves it ambiguous.

Both `Traffic` and `Response` write through to an open details view as
well as the list entry — the details view holds a *snapshot* taken at
open time, so without that the user would have to close and reopen the
row to see anything land.

## Details view scrolling

The details body is a `Paragraph` with wrapping, so the scrollable
height isn't `lines.len()` — one long header can occupy four rows. The
honest measurement is `Paragraph::line_count(width)`, which lives behind
ratatui's `unstable-rendered-line-info` feature; that feature is now
enabled in the workspace manifest.

Enabling an explicitly-unstable API deserves a note. The alternative was
reimplementing ratatui's wrap algorithm to count rows ourselves, which
fails in a worse way: our count and ratatui's would drift apart silently
and the view would either refuse to reach its last line or scroll past
it. The unstable API is one call at one site, so a breaking change is
cheap to absorb.

Max scroll is computed during render (the only place the wrap width is
known) and stashed in a `Cell` for the key handler to clamp against.
The offset is clamped again at render time, so a resize that shortens
the content can't strand the view past the end.

## Selection mode in the details view

Clicking the details body releases mouse capture, exactly like clicking
the Sandbox body — so headers can be dragged over and copied with the
terminal's own copy shortcut. The hit test runs after the `×` and
sub-tab tests so those keep working with the mouse.

Discoverability is the weak point of a click-to-activate gesture with no
visual affordance, so the panel footer carries a `click to select text`
hint while details are open.

While capture is released the mouse belongs to the terminal, so the
wheel and the `×` don't work until `Esc` restores it. That's inherent to
the mechanism rather than something worth papering over — it's how the
Sandbox tab has always behaved.

## Verification

`mise run test` (192, up from 180) and `mise run lint` pass. The new
tests cover transfer formatting and column-width invariants, `Traffic`
and `Response` pairing (including the evicted-entry and
open-details-snapshot cases), and scroll clamping.

Not covered by tests: the counting transport wrapper and the response
emission are only exercised end-to-end, since both need a live proxy
connection. The rendering itself is untested, as elsewhere in this
crate.
