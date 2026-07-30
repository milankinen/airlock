# Fix monitor TUI crash when the terminal shrinks to a few rows

## Symptom

Shrinking the terminal window running the monitor to 1–2 rows tall killed the
TUI (panic in debug builds; grid corruption then a panic on the next byte in
release).

## Root cause

`ui::body_area` returns an empty `Rect` when the terminal is shorter than the
bottom tab bar (`height < 3`). Both the startup sizing and the `Resize` event
handler passed that body straight to `sink.resize(0, 0)`. vt100's grid
computes `rows - 1` on a `u16` in `set_size`, so a zero-row resize underflows —
panicking in debug and, in release, wrapping to 65535, sizing the row vector
to zero, and panicking on the next guest byte.

## Fix

Guard both `sink.resize` call sites: only resize (and, in the event handler,
only forward the size to the guest PTY) when the body area is non-empty. When
the terminal is too small the sink keeps its last valid size and the render
path clips, so the TUI simply shows nothing useful until the window grows
again — no crash.

## Tests

`ui::tests::body_area_is_empty_for_tiny_terminal` pins the invariant the guard
relies on: `body_area` is empty for every height up to the tab-bar height and
non-empty one row above it.
