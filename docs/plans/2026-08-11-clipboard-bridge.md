# Clipboard bridge (guest ↔ host)

## Context

Programs inside the sandbox have no clipboard. There is no `pbcopy`, no
`xclip`/`wl-copy`, and no `DISPLAY`/`WAYLAND_DISPLAY`, so anything that
shells out to a clipboard tool fails. The observed case is Claude Code,
whose copy silently does nothing in the VM.

Reverse-engineering that binary produced two facts that shape this design:

- Its **copy** path calls the native tool *and* emits OSC 52 unconditionally.
  So copy already has a second route (a vt100 callback in `airlock-monitor`
  would carry it). The native bridge is the belt to that braces, and the only
  route when the terminal or a `tmux` layer eats OSC 52.
- Its **paste** path is `xclip -selection clipboard -t text/plain -o || wl-paste`
  — a plain fallback chain with no env gate. **Paste has no other route at all**,
  because OSC 52 reads are refused by most terminals (and rightly so).

Paste is therefore the capability this feature uniquely provides.

The tool-selection gate on `WAYLAND_DISPLAY`/`DISPLAY` is one code path in one
program, not a Linux convention. Airlock will **not** synthesize a display env
var; the manual documents the existing `[env]` section for programs that gate
on one.

## Security posture

Both directions are holes in the sandbox and both default to `false`.

- **copy (guest → host)** — sandboxed code writes arbitrary bytes to the host
  clipboard. The risk is *content*, not size: a later paste into a shell can
  execute. `copy_limit` bounds memory/DoS, not injection.
- **paste (host → guest)** — the dangerous one. It is a **guest-initiated read**:
  sandboxed code can poll the host clipboard at will, with no user action, and
  take whatever was last copied (password-manager entries, tokens). "The host is
  trusted" is true of the *data* but not of the *capability*. Docs must say this
  plainly.

Enforcement lives on the host, not in the guest shim:

- When a direction is disabled the host **does not pass the capability**, so a
  compromised guest has nothing to invoke.
- The host re-checks the per-direction flag and the size cap on every call.

## Approach

Four pieces. airlockd can already write into the container rootfs at
`/mnt/overlay/rootfs` (the pattern `app/airlockd/src/net/host_socket_forward.rs:49`
uses), so shims need no host-side file mount — airlockd creates them, only when
the capability was granted.

**Transport: a FIFO pair, not a unix socket.** A shell shim cannot open a unix
socket without `nc`/`socat`, which minimal Debian images don't ship; `cat > fifo`
and `cat fifo` need nothing. Paste works because airlockd blocks opening the
write end and fetches from the host each time the guest opens the read end.

### 1. Config — `app/airlock-cli/src/config.rs`

Add `#[config(nest)] pub clipboard: Clipboard` to `Config` (alongside `disk`,
`network`), and the section struct. Repo convention: snake_case keys.

```rust
pub struct Clipboard {
    #[config(default_t = false)]     pub copy: bool,
    #[config(default_t = ByteSize(1024 * 1024))] pub copy_limit: ByteSize,
    #[config(default_t = false)]     pub paste: bool,
}
```

`ByteSize` is already imported and used for `vm.memory` / `disk.size` — reuse it
so `copy_limit = "2MB"` parses for free.

### 2. Schema — `app/airlock-common/schema/supervisor.capnp`

```capnp
interface Clipboard {
  copy  @0 (data :Data) -> ();
  paste @1 () -> (data :Data);
}

struct ClipboardConfig {
  copy  @0 :Bool;
  paste @1 :Bool;
  sink  @2 :Clipboard;   # null unless at least one direction is granted
}
```

Append `clipboard :ClipboardConfig` as the **last** parameter of `Supervisor.start`
(params are implicitly numbered by position — append only, never reorder). The
default-initialised value — both flags false, null `sink` — is exactly the
disabled state, so "not granted" needs no sentinel.

### 3. Host — new `app/airlock-cli/src/clipboard.rs`

- `detect() -> Option<HostTool>`: macOS → `pbcopy`/`pbpaste`; Linux → `wl-copy`/
  `wl-paste`, then `xclip`, then `xsel`. Probe with a `PATH` lookup.
- `HostTool::copy(&[u8])` / `paste() -> Vec<u8>`: spawn the program, pipe stdin/stdout.
- `ClipboardImpl`: the capnp server. Holds the tool + both flags + the cap.
  `copy()` rejects when `!copy` or `data.len() > copy_limit`; `paste()` rejects
  when `!paste`. Rejections are capnp errors and are logged host-side.
- Wire into `app/airlock-cli/src/rpc/supervisor.rs::start()` exactly like
  `LogSinkImpl` (line ~164): `capnp_rpc::new_client(ClipboardImpl { .. })`, set
  `sink` only when a direction is enabled.
- Graceful degradation in `app/airlock-cli/src/cli/cmd_start.rs`: if a direction
  is enabled but `detect()` returns `None`, warn via `crate::cli::log!` and leave
  that direction off. Never hard-fail a `start` over the clipboard.

### 4. Guest — new `app/airlockd/src/clipboard.rs`

Setup (sync, from `init::setup` in `app/airlockd/src/init/linux.rs`), skipped
entirely when `sink` is null:

- `mkfifo` at `/run/airlock/clipboard.copy` and `.paste`, mode 0600, resolved
  with the existing `crate::util::resolve_in_root(Path::new("/mnt/overlay/rootfs"), …)`.
- Write shims to `/usr/local/bin/{wl-copy,wl-paste,xclip,xsel}`, mode 0755
  (`/usr/local/bin` is first in the container `PATH` from `oci.rs:379`).

Ship all four names: paste's fallback chain tries `xclip` **first**, copy's probe
prefers `wl-copy`, so both names are needed to serve both directions.
`xclip`/`xsel` dispatch on `-o`. A shim for a disabled direction must `exit 1`
so `||` chains fall through rather than hanging.

```sh
#!/bin/sh
# xclip
for a in "$@"; do [ "$a" = "-o" ] && exec cat /run/airlock/clipboard.paste; done
exec cat > /run/airlock/clipboard.copy
```

Serve loops (async, spawned from `app/airlockd/src/main.rs` after `init::setup`,
alongside the existing services; capability read in `app/airlockd/src/rpc.rs`
start handler next to `log_sink`, line ~222):

- **copy**: reopen the read end in a loop; read at most `copy_limit + 1` bytes so
  an oversized write is detected without buffering it; call `sink.copy()`.
- **paste**: open the write end (blocks until a guest reader opens); call
  `sink.paste()`; write; close; loop.

Serialize each loop — one open at a time — so concurrent shim invocations can't
interleave bytes.

Two implementation notes:

- `libc` is already an airlockd dependency and `app/airlockd/src/net/tun.rs:144`
  already calls `libc::mknod` — `libc::mkfifo` follows that precedent, no new dep.
- **FIFO opens block, which tokio will not like.** Opening the write end waits for
  a reader, and the read end waits for a writer. Do the opens on
  `spawn_blocking`, or open `O_NONBLOCK` and register with tokio's async fd —
  a naive blocking open inside `spawn_local` stalls the whole guest runtime,
  which is PID 1. This is the single most likely way to get this wrong.

### 5. Docs

- New `docs/manual/src/configuration/clipboard.md` + `SUMMARY.md` entry. Must cover:
  the security posture above (paste especially), the `[env]` one-liner for tools
  that gate selection on a display var, and the caveat that in-process clipboard
  libraries speaking X11/Wayland directly (Rust `arboard`, etc.) never shell out,
  so no shim can serve them.
- Dev log entry `docs/log/2026-08-11-clipboard-bridge.md`.

## Verification

`mise run format`, `mise run test`, `mise run bats:cli`, `mise run bats:vm`.

**Automated tests must not touch the developer's real clipboard.** Assert
structure and plumbing, not host clipboard state:

- Unit (`cargo test`): config defaults and `copy_limit = "2MB"` parsing; a typo'd
  key fails loudly; `detect()` fallback order; `ClipboardImpl` rejects over-limit
  data and rejects a disabled direction; shim script generation (string snapshot).
- bats CLI (`tests/cli/`): `airlock show` reports clipboard off by default.
- bats VM (`tests/vm/clipboard.bats`): with `copy = true`, the four shims exist,
  are executable, and are on `PATH`; with clipboard absent from config, none exist
  and neither FIFO is present.

Manual end-to-end (not in CI, clobbers the real clipboard):

1. `[clipboard] copy = true` → in the guest, `echo hello | wl-copy`, then paste on
   the host.
2. `paste = true` → copy something on the host, then `wl-paste` in the guest.
3. `printf '%0.sx' $(seq 2000000) | wl-copy` → rejected, host clipboard unchanged,
   warning logged.
4. With `copy = false`, confirm `/usr/local/bin/wl-copy` does not exist in the guest.
5. Claude Code specifically: paste should work with no extra config; copy needs
   `WAYLAND_DISPLAY` in `[env]` — verify both the with- and without-`[env]` behaviour
   matches what the manual claims.
