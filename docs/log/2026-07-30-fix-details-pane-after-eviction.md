# Fix network details pane going stale after its row is evicted

## Symptom

With a connection's details pane open, if more than the buffer cap
(default 100) of newer connections arrived, the pane froze: it kept
showing `State: Open` with stale `Sent`/`Received` byte counts and never
picked up the real disconnect time or final transfer totals, even though
the connection had long since closed. The same staleness affected an open
request's Response section once enough newer requests pushed the original
out of the live list.

## Root cause

The open details view (`NetworkTab::details`) is a snapshot cloned at open
time and keyed by entry `id`. It is a separate field from the capped
`connections`/`requests` vecs, so it survives eviction — but in
`push_event`, the code that wrote later `Disconnect`, `Traffic`, and
`Response` updates through to that snapshot was nested inside the live-list
`connections.iter_mut().find(...)` (respectively `requests...`) lookup.
Once the buffer cap (`cap_entries`) evicted the selected entry from the
live vec, `find` returned `None`, the whole block was skipped, and the
snapshot stopped receiving updates.

## Fix

Hoisted the snapshot write-through out of the live-list `if let Some(entry)
= …find(…)` block in all three arms. The live-list entry is still updated
when present, and the snapshot is now updated independently, matched by its
tracked `id`, so it stays current even after its underlying row is evicted.
The `open.id == info.id` guard is unchanged, so unrelated connections are
never touched. Behavior for the non-evicted case is identical.

## Tests

Added `traffic_updates_open_details_after_eviction` and
`disconnect_updates_open_details_after_eviction`: open a connection's
details, force-evict its row via `max_tcp_connections = 1`, then push a
`Traffic` / `Disconnect` for the evicted id and assert the snapshot
reflects it. Both fail before the fix and pass after. The existing
`traffic_for_evicted_connection_is_ignored` still passes (no details open),
confirming no cross-row bleed.
