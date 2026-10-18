# Host MCP servers

Some MCP servers must run on the host (`mise mcp`, tools that need host
state). Agents in the sandbox could not reach them.

## Change

A config entry `[mcp.<name>]` (`command`, `args`, `env`, `enabled`)
describes a stdio MCP server. The sandbox gets it at
`http://mcp.airlock/<name>` as MCP Streamable HTTP. `GET http://mcp.airlock/`
lists the names. See the manual page `configuration/mcp.md`.

```
agent ──HTTP──▶ mcp.airlock (airlockd) ──capnp McpHost.open──▶ airlock-cli ──stdio──▶ child
```

## Decisions

- **No MCP crate.** `rmcp` can build a proxy only with typed handlers. It
  drops unknown methods and notifications and does not forward
  server-to-client requests (sampling, elicitation). The proxy crates built
  on it have little use and many dependencies. We pass `serde_json::Value`
  through. The code uses only axum, tokio and serde_json, which airlockd
  already has.
- **Streamable HTTP lives in the guest.** The host relays JSON-RPC lines
  only. The capnp surface is small (`McpHost.open`, `McpSession.send`,
  `McpSink.message/closed`). All HTTP details stay in one place.
- **One process per MCP session.** A stdio server expects one client. We
  need no id rewriting and sessions are isolated. The process stops when
  the client sends DELETE, when the child exits, or when the VM ends.
- **Capability grant.** The host passes an `McpHost` capability in `boot`.
  It is null when no server is enabled. The guest can open only names from
  the host config. The host checks the name on each `open`.
- **Vhost dispatch with back-compat.** The local HTTP server on
  `127.0.0.1:80` sends only `Host: mcp.airlock` to MCP. All other hosts
  (`admin.airlock`, `localhost`, IPs) go to the admin router as before.
  Guest DNS reserves `mcp.airlock` like `admin.airlock`.
- **`!Send` bridge task.** Axum handlers are `Send`. Capnp clients are
  `Rc`. One `spawn_local` bridge task owns the capnp objects. Handlers talk
  to it with `mpsc` and `oneshot` channels. SSE bodies read from `mpsc`
  receivers.
- **Process group kill.** The host starts each child in its own process
  group with `kill_on_drop`. On Linux the child also gets a parent-death
  signal. Dropping the session kills the whole group, so helper processes
  of the server do not stay behind.
- **Idle timeout and session limit.** `timeout` (seconds, default 600,
  `0` = none) and `max_sessions` (default 50, minimum 1) for each
  server. A session is idle when no message went in either direction
  for `timeout`. The host does not track open requests: a guest could
  keep a session alive with requests that the server never answers, and
  the set of open requests would have no limit. Thus a tool call that
  sends nothing for longer than `timeout` stops. Users with slow tools
  set a larger `timeout`. Over the limit, the host stops the session
  with the oldest activity, also a busy one. Otherwise a guest could
  keep the limit full. The host makes room only after the new session
  started, so a failed start stops no session. The guest gets `closed`
  with the reason, then answers 404, and the client starts a new
  session.
- **Limits.** The host limits the line length from the child (16 MiB) so a
  bad server cannot make the host buffer without bound. The host sends the
  child stderr to the tracing log.

## Security

- The server runs on the host with the rights of the user, outside the
  sandbox. Any process in the sandbox can call its tools. The manual says
  so.
- `[mcp.*]` is part of the config values. The config-change prompt
  protects it, including entries from packs.
- Env templates (`${VAR}`) resolve on the host at sandbox start. Values
  never go to the guest. An undefined variable fails the start.
- The pack install boot disables MCP.
- The guest checks the `Origin` header (DNS-rebinding guard from the MCP
  spec) and uses a 128-bit random session id.

## Not supported

- The legacy SSE transport.
- Resumable streams.
- JSON-RPC batches (the 2025-06-18 spec removed them).

## Tests

- Host: real child process through an RPC loopback.
- Guest: real axum server with a fake `McpHost`.
- bats: config validation errors (`tests/cli/config.bats`) and an end-to-end
  VM test (`tests/vm/mcp.bats`).
