# Merge resize into Stdin RPC + supervisor refactor

### What

Replaced ByteStream with a dedicated Stdin RPC interface that carries
both keyboard data and terminal resize events in a single stream.
Refactored supervisor process management to use channel-based
communication between the main loop and the RPC ProcessImpl.

### Why

Having `resize` as a separate method on `Process` required splitting
the PTY into read/write halves and sharing the writer via
`Rc<RefCell<Option<OwnedWritePty>>>` between the stdin relay task and
resize handler. This interior mutability caused problems when resize
was called while stdin relay was writing.

By merging resize into the stdin stream (`ProcessInput` union of
`stdin: DataFrame | resize: TermSize`), the PTY writer stays in a
single task that reads from the Stdin RPC and dispatches both data
writes and resize calls — no sharing needed.

### Design

**Schema changes:**

- Removed `ByteStream` interface entirely
- Added `Stdin { read() -> ProcessInput }` interface
- Removed `resize` from `Process` (now poll/signal/kill only)
- Removed `err` variant from `DataFrame` (just eof/data)
- `Supervisor.start()` takes `Stdin` instead of `ByteStream`

**CLI side:**

- New `StdinImpl` (stdin::Server) multiplexes tokio stdin reads and
  SIGWINCH resize signals via `tokio::select!` in a single `read()`
- Removed separate resize loop from main — resize events flow through
  the Stdin capability automatically
- Removed `OutputStream`/`InputStream` stream abstractions from
  protocol crate (no longer needed without ByteStream)

**Supervisor side:**

- `ProcessImpl` now communicates with `attach()` via channels:
    - `frames` channel: PTY output + exit code → ProcessImpl.poll()
    - `signals` channel: ProcessImpl.signal()/kill() → attach() loop
- `attach()` owns the main select loop (PTY reads + signal dispatch)
- `relay_stdin()` extracted as standalone async fn
- PTY read errors (EIO on child exit) break the loop correctly
- Supervisor hangs after sending exit frame; CLI kills VM to stop it

**Build:**

- Added Docker volume for supervisor target dir to cache incremental
  builds across `mise run build:supervisor` invocations
