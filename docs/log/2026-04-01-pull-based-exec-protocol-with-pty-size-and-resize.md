# Pull-based exec protocol with PTY size and resize

### What

Redesigned the RPC protocol to pull-based: CLI calls `proc.poll()`
for output, supervisor calls `stdin.read()` for input. Added optional
PTY config (size or none) to `exec`, `resize` to `Process`, and
SIGWINCH handling to propagate terminal resizes.

### Schema

```
exec(stdin :ByteStream, pty :PtyConfig) -> (proc :Process)
Process { poll, signal, kill, resize }
ByteStream { read -> DataFrame(eof|data|err) }
ProcessOutput(exit|stdout|stderr)
PtyConfig(none|size(rows,cols))
```

### Key decisions

- **Pull-based read()** over push-based write() — cleaner API, inline
  error handling, natural backpressure. Vsock latency is negligible.
- **Optional PTY** — `pty: none` for future non-interactive exec
  (separate stdout/stderr). Shell always gets a PTY.
- **SIGWINCH** → `proc.resize()` — spawned as `spawn_local` task
  alongside the poll loop. No-op if process has no PTY.
- **Removed old console relay** — all I/O goes through RPC now.
