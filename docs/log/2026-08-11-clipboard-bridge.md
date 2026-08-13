# Add `[clipboard]` bridging the sandbox to the host clipboard

## Motivation

The sandbox had no clipboard at all. Any program that shells out to
`pbcopy`, `xclip`, `wl-copy` or friends found nothing on `PATH`, and there
was no display server for them to talk to even if it had. The failure was
silent: copy appeared to work and nothing arrived.

Tracing the reported case (Claude Code copying nothing from inside a
sandbox) turned up two things worth recording, because they shaped the
design more than the original request did.

First, its copy path calls the native tool *and* emits OSC 52
unconditionally — so copy already has a second route, and a terminal that
honours OSC 52 would carry it. Paste has no such route: the read form of
OSC 52 is refused by most terminals, and rightly so. **Paste is the
capability this feature uniquely provides**, which also makes it the one
that needed the most careful gating.

Second, its two directions disagree with each other about how to find a
tool. Copy runs a probe that requires `WAYLAND_DISPLAY` or `DISPLAY` to be
set before it will select `wl-copy`/`xclip`/`xsel` at all; paste is a plain
`xclip … || wl-paste` fallback chain with no such check. So the env-var
requirement is one code path in one program — not a Linux convention — and
airlock does not fake a display for it. Doing so would assert to every
other program in the sandbox that a compositor exists. The manual documents
the one-line `[env]` opt-in instead.

## Change

`[clipboard]` grants each direction separately, both off by default:

```toml
[clipboard]
copy       = false
paste      = false
copy_limit = "1 MB"
```

Enabling a direction installs shims for all four tool names (`wl-copy`,
`wl-paste`, `xclip`, `xsel`) into the container at `/usr/local/bin`, which
is first on its `PATH`. All four go in whenever either direction is on,
because — as above — even a single program disagrees with itself about
which name to reach for. A shim for an ungranted direction exits non-zero
rather than blocking, so the `a || b` chains callers use fall through
instead of hanging on a pipe nobody serves.

Transport is a **FIFO pair**, not a unix socket: a shell shim cannot open a
socket without `nc`/`socat`, which minimal images do not ship, whereas
`cat > fifo` needs nothing. Framing falls out of FIFO semantics — one
open-to-EOF cycle is exactly one clipboard operation, so consecutive
`wl-copy` calls cannot run together into one blob.

Enforcement is host-side. An ungranted direction is not passed to the guest
as a capability at all, so a compromised sandbox has nothing to invoke; the
booleans are a capability grant, not a flag the guest could subvert. The
host re-checks both grants and the size cap on every call.

The guest bounds its read too, which is not redundant: `airlockd` is PID 1,
and an unbounded `read_to_end` on a guest-fed FIFO would let
`cat /dev/zero > fifo` grow init until the VM dies. It drains to EOF (so the
writer is never left blocked on a full pipe) but stops retaining past the
limit, making that attack cost constant memory. `ClipboardConfig.limit`
exists in the schema for this reason alone.

Where a display variable *is* genuinely meaningful — the host — it is
honoured: `wl-copy` is only selected when the host has `WAYLAND_DISPLAY`,
`xclip`/`xsel` only with `DISPLAY`. A host with no usable clipboard program
downgrades to ungranted with a warning rather than failing the start.

## Limits

The bridge replaces clipboard *programs*, so it only serves software that
shells out to one. Applications linking a clipboard library that speaks X11
or Wayland directly (Rust's `arboard`, say) never spawn a subprocess and
are unreachable this way.
