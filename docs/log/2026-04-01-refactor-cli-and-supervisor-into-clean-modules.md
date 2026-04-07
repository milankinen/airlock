# Refactor CLI and supervisor into clean modules

### What

Split the monolithic main.rs files into focused modules. Introduced
anyhow for error handling. Added protocol stream wrappers
(`OutputStream`/`InputStream`) with standard Rust async traits.

### CLI structure

```
cli/src/
  main.rs        — 50 lines: parse args, create config, run, handle exit
  config.rs      — Config struct with defaults
  error.rs       — CliError { Expected, Unexpected(anyhow) }
  terminal/      — TerminalGuard + SIGWINCH resize signal
  vm/            — vm::create(config) → (VmHandle, OwnedFd)
  rpc/
    client.rs    — Client: connect, exec
    process.rs   — Process: poll, resize; ProcessEvent enum
```

### Supervisor structure

```
sandbox/supervisor/src/
  main.rs        — 20 lines: vsock listen, serve
  vsock.rs       — OwnedFd-based listen/accept
  rpc/
    server.rs    — SupervisorImpl, serve()
    process.rs   — spawn(), ProcessImpl
```

### Protocol streams (`protocol/src/streams.rs`)

- `OutputStream`: boxed `dyn AsyncRead` → `byte_stream::Client` via
  `From` conversions. No generic param.
- `InputStream`: `byte_stream::Client` → `impl AsyncRead`. Stateful
  poll-based implementation that properly persists pending RPC futures
  between polls. Internal buffer for partial reads.

### Key decisions

- **anyhow for supervisor** — all errors are unexpected, no need for
  typed error enum.
- **CliError { Expected, Unexpected }** — Expected errors show clean
  messages, Unexpected use anyhow's `{:#}` formatting with context.
- **`LocalSet::run_until()`** not `enter()` — `enter()` only sets
  spawn context but doesn't drive tasks. `run_until()` both sets
  context AND polls spawned tasks, required for capnp-rpc's
  `spawn_local` to work.
- **`InputStream` implements `AsyncRead`** not `Stream` — more
  fundamental trait, `Stream` can be derived via
  `tokio_util::io::ReaderStream` if needed later.
