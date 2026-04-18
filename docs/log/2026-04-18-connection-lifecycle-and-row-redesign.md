# Connection lifecycle + network row redesign

## What changed

On the F2 Monitor's network panel:

- **Gray header row** on both sub-tabs (Requests, Connections) so the
  columns are self-describing.
- **Requests layout**: `Received at · Endpoint · Target · Result`. No
  bullet — the focus is the endpoint (method + path), which now takes
  all the remaining horizontal space and truncates with `…`. The
  target column auto-sizes to the widest current entry (clamped to
  12..30 cols) so headers and rows stay aligned.
- **Connections layout**: `⦿ · Target · Connected at · Disconnected
  at · Result`. Target is white (it's the "beef" of the row) and
  takes the expand slot.
- **Connection lifecycle**: TCP connections live for a while, so the
  panel now tracks whether each is still open.
    - `⦿` green — allowed and still open; `Disconnected at` blank.
    - `⦿` gray — allowed, closed; `Disconnected at` filled in.
    - `⦿` red — denied (always closed).

## Event shape

Two additions to the `NetworkEvent` stream:

```rust
pub enum NetworkEvent {
    Connect(Arc<ConnectInfo>),     // now carries `id: u64`
    Disconnect(Arc<DisconnectInfo>), // new
    Request(Arc<RequestInfo>),
}

pub struct DisconnectInfo {
    pub id: u64,
    pub timestamp: SystemTime,
}
```

- IDs are a per-process `AtomicU64` counter on `Network`. No UUIDs —
  the id never crosses the process boundary, and `uuid` isn't already
  a direct dep of either crate. A monotonic `u64` is smaller, cheaper,
  and sufficient for the pairing job.
- Emission stays receiver-gated on `broadcast::Sender::receiver_count()`
  so non-monitor runs do zero allocation (same invariant as the
  Connect/Request path).

## Where the emissions live

- `Connect` now fires from `network::server::connect()` once per TCP
  RPC, right after `resolve_target`. Previously it fired from inside
  `resolve_target` itself; moving it up pairs more cleanly with the
  lifecycle and lets us mint the id once per guest connect.
- `Disconnect` fires inline at the end of `spawn_tcp_connection`'s
  spawned task, after `handle_connection` returns (whether Ok or Err).
  This is the natural close point — the relay has unwound, the guest
  has seen EOF/403/close, and any further bytes would be writes into
  an already-shutting-down transport.
- Unix-socket denies also emit a Connect + immediate Disconnect (same
  id). Successful socket connections don't emit lifecycle events — the
  socket path isn't currently surfaced in the connections log, and
  plumbing a lifecycle through the socket task would be extra work for
  no current UX gain.

## Trade-offs considered

- **IDs vs. pointer-equality / vec-index matching**: if the id-carrying
  `ConnectInfo` got evicted by the per-list cap before the matching
  `Disconnect` arrived, a pointer/index scheme would dangle. With an
  id the tail just silently drops — `push_event`'s find-by-id returns
  `None` and the Disconnect is a no-op. Clean.
- **Emitting Disconnect from a Drop impl on a wrapper type** vs the
  inline emission we picked: a Drop impl would be more general (covers
  panic unwinds too) but forces the events channel through another
  struct. The spawned task already owns the end-of-life point; adding
  four lines there is simpler than introducing an RAII guard.
- **Open-row selection stays "follow newest"** on new Connect events;
  Disconnect events don't touch selection at all. That's the right
  behavior — a Disconnect is a mutation of an existing row, not a new
  row, so jumping the cursor would be disorienting.

## TUI-side plumbing

- `ConnectionEntry` picks up `id: u64` and
  `disconnected_at: Option<SystemTime>`.
- `push_event` gains a third arm:
  ```rust
  NetworkEvent::Disconnect(info) => {
      if let Some(entry) = self.connections.iter_mut().find(|c| c.id == info.id) {
          entry.disconnected_at = Some(info.timestamp);
          if let Some(DetailView::Connection(open)) = self.details.as_mut()
              && open.id == info.id
          {
              open.disconnected_at = Some(info.timestamp);
          }
      }
  }
  ```
  The second branch keeps the Details sub-tab in sync — if the user
  had drilled into a row and the connection then closes, they see the
  transition without re-opening.
- Row rendering moved out of `row::build_row` (the old shared
  skeleton) into per-sub-tab `build_request_row` / `build_connection_row`.
  `row.rs` now just hosts column constants (`BULLET_COLS`,
  `TIMESTAMP_COLS`, `RESULT_COLS`), the padding/truncation utilities,
  and `apply_row_highlight`. Each widget inlines its own header row
  alongside its row builder so the column widths and inter-column
  gaps stay in lockstep (a shared header helper couldn't express the
  2-space gaps after the bullet and between Connected/Disconnected).

## Per-tab allowed/denied counters

The panel footer used to show a single `allowed_count` / `denied_count`
pair covering every event type. With two sub-tabs and a 100-entry cap
per list, that was both confusing (the count mixed Requests with
Connections) and misleading (the user asked for counters that persist
past buffer eviction, not just the visible rows).

`NetworkTab` now carries four counters — `connection_allowed`,
`connection_denied`, `request_allowed`, `request_denied` — incremented
in `push_event` *before* `cap_entries` runs, so they accumulate over
the full run. `visible_counts()` returns the pair for the active
sub-tab (Details falls back to its parent sub-tab); the Monitor tab
header's badge uses `total_count()` (all four summed) as a global
activity hint.
