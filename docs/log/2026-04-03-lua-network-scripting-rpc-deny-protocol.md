# Lua network scripting + RPC deny protocol

### What

Scriptable network filtering via inline Lua (LuaJIT) rules in config.
Each outbound connection runs through all tcp_connect rules. Deny
short-circuits; allow must be explicit or fall back to default_mode.

### Rule configuration

Rules defined as `[[network.rules]]` in ez.local.toml with name, type
(tcp_connect/http_request enum), required env vars with descriptions,
and inline Lua script. Env vars are validated at startup (hard error
if missing) and snapshotted into per-rule `env` table.

### Lua sandbox

Each rule gets its own Lua VM (mlua + LuaJIT). Dangerous globals
removed (os, io, debug, load, require). Instruction limit via hook
(1M instructions). Scripts compiled once via `into_function()`, called
per connection.

### Lua API

- `req.host/port/tls` — read/write (tls read-only)
- `req:allow()`, `req:deny()` — set permission
- `req:hostMatches(pattern)` — glob match (e.g. "*.github.com")
- `env.VAR_NAME` — declared env vars only
- `log(msg)` — tracing debug output

### Permission model

1. All tcp_connect rules execute in order
2. If any calls `req:deny()` → denied immediately
3. After all rules: if any called `req:allow()` → allowed
4. Otherwise: use `network.default_mode` (allow/deny, default deny)

Initial `allowed` state matches default_mode, so with `allow` mode
scripts only need to deny.

### RPC protocol extension

`NetworkProxy.connect` now returns `ConnectResult` union (server/denied)
instead of bare TcpSink. Supervisor proxy handles denied responses by
shutting down the container's connection cleanly with a debug log.

### CLI refactoring

Module `mod.rs` files converted to `<module>.rs` where possible.
Network scripting under `network/scripting/` with `connect_request.rs`
as separate userdata type (extensible for future http_request type).
