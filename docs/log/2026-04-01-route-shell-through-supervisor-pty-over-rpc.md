# Route shell through supervisor PTY over RPC

### What

Shell I/O now flows through the supervisor via Cap'n Proto RPC instead
of the virtio console relay. The supervisor spawns a shell on a proper
PTY (`/dev/pts/0`), eliminating the "can't access tty; job control
turned off" warning.

### Architecture

```
CLI stdin → RPC stdin.write() → supervisor → PTY master → shell
CLI stdout ← RPC stdout.write() ← supervisor ← PTY master ← shell
                                  stdout.done(exitCode) on shell exit
```

### Schema

```capnp
interface Supervisor {
  openShell (rows, cols, stdout :OutputStream) -> (stdin :OutputStream);
}
interface OutputStream {
  write (data :Data) -> stream;
  done (exitCode :Int32) -> ();
}
```

Both directions use push-based `OutputStream` callbacks over the same
vsock connection. Cap'n Proto RPC multiplexes everything automatically.

### Key decisions

- **Push-based both directions** — CLI passes `stdout` callback (receives
  output), gets back `stdin` capability (sends input). No polling, no
  second vsock port. capnp-rpc handles multiplexing.
- **`pty-process` crate** — replaces ~100 lines of raw libc (`openpty`,
  `fork`, `setsid`, `ioctl`, `dup2`, `execl`, `fcntl`, `AsyncFd`)
  with ~30 lines. Integrates with tokio `AsyncRead`/`AsyncWrite`.
- **Exit code propagation** — supervisor awaits `child.wait()`, sends
  exit code via `stdout.done(exitCode)`, CLI receives via oneshot
  channel and calls `std::process::exit(code)`.
- **Bookworm for Docker builder** — bullseye's capnp 0.7.0 doesn't
  support `-> stream` syntax. Bookworm has 0.9.2.
