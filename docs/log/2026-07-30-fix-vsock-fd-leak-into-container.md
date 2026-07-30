# Fix host RPC vsock fds leaking into container processes

## Symptom

Every process launched inside the sandbox inherited airlockd's live vsock
file descriptors — the supervisor and network-proxy RPC channels to the host.
An untrusted process could walk `/proc/self/fd`, find the connected
`AF_VSOCK` fds, and read in-flight RPC traffic (other processes' stdio, exec
commands) or inject Cap'n Proto frames straight to the host, escaping the
mediation airlockd is supposed to enforce.

## Root cause

The vsock listener was created with `socket(2)` and connections accepted with
`accept(2)`, neither of which sets close-on-exec. tokio's `from_std` /
`from_raw_fd` wrapping does not add it either, so the fds stayed open across
the `fork`+`exec` that spawns container processes (container processes are not
put in a PID/fd-isolating namespace that would otherwise help).

## Fix

- Create the listener with `SOCK_STREAM | SOCK_CLOEXEC`.
- Accept connections with `accept4(…, SOCK_CLOEXEC)` instead of `accept`.

The accepted fd is later moved into a `TcpStream` via `into_raw_fd()` /
`from_raw_fd()`; `FD_CLOEXEC` is a property of the descriptor in the fd table
and is preserved across that hand-off, so both the listener and the live
connection are now close-on-exec. This is invisible to the async runtime
(epoll registration is unaffected) and only changes exec inheritance.

## Scope / follow-ups

Audited the other raw fds in airlockd:

- `process.rs` pipes already use `pipe2(O_CLOEXEC)`.
- The TUN device (`net/tun.rs`) is opened via `OpenOptions`, which Rust std
  opens `O_CLOEXEC` by default — so the container cannot inherit the raw
  network device fd either.
- `disk.rs` uses `std::fs::File::open` (also `O_CLOEXEC`).

One non-critical gap remains outside this fix: `init/linux/overlay.rs` opens
`/dev/kmsg` with a raw `libc::open` lacking `O_CLOEXEC` (a read-only kernel-log
fd). It is a low-severity info-leak, tracked separately.
