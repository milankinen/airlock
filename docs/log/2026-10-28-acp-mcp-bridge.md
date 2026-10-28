# ACP MCP bridge

Editors send `mcpServers` in ACP session requests. The entries name host
commands and `localhost` URLs. They fail in the VM, and their `env` and
`headers` secrets would go into the VM. Plan:
[2026-10-28-acp-mcp-bridge.md](../plans/2026-10-28-acp-mcp-bridge.md).

## Change

- `airlock acp --proxy-mcp-servers`. Without it, the proxy removes all
  `mcpServers` entries (the agent gets `[]`) and the `initialize` result
  has no `mcpCapabilities`. With it, the result shows `http` and `sse`.
- The proxy bridges stdio, http and sse entries. It drops `acp`, unknown
  and invalid entries with a warning in `acp.log`. With
  `--pass-client-capabilities`, it keeps `acp` and unknown entries as
  they are.
- The agent gets
  `{"type":"http","name":<orig>,"url":"http://acp.mcp.airlock/<key>/<name>","headers":[]}`.
- The proxy drops all bridged entries when the agent has no http MCP
  support. The session still runs.
- New capnp calls: `CliService.addMcpServer` and
  `Supervisor.addMcpServer`, both with an `McpGrant`.
- Manual: `tips/editor-acp.md`, `configuration/mcp.md`,
  `technical/networking.md`, `technical/rpc.md`.

```
editor -> airlock acp -> cli.sock -> airlock start -> vsock -> airlockd
agent -> http://acp.mcp.airlock/<key>/<name> -> airlockd -> McpHost.open -> airlock acp -> stdio child | remote URL
```

## Design

- **Capability chain.** `airlock acp` implements `McpHost`. It passes
  the capability over the CLI service to `airlock start`. That process
  checks `session` and `name` (1-64 chars of `[A-Za-z0-9_-]`) and
  forwards the capability and the grant unwrapped to airlockd.
- **Grants.** `McpGrant.release` removes the route, ends its sessions
  and returns after they ended. Dropping the grant does the same without
  waiting. When `airlock acp` exits, the CLI connection closes and the
  grants release.
- **Per-session proxy keys.** Each ACP session gets a random key. Two
  sessions, also of different proxies, never share a URL. `session/load`
  and `session/resume` of a known session keep the key. The proxy first
  releases the old set, then registers the new set. `session/close`,
  an error reply, agent exit and client end release the key.
- **Registration in the proxy loop.** Registration is a future in the
  main `select!` loop. The session request and later client lines wait
  in order. All servers of a request register in parallel with one
  shared limit of 10 s. A server that fails or times out is dropped.
- **Guest routes.** The airlockd bridge owns the route map. Config
  servers stay at `mcp.airlock/<name>`. Run-time servers use the host
  name `acp.mcp.airlock` and the path `/<session>/<name>`. The call is
  refused before boot, as for `bindHostPort`. A release also ends opens
  in flight.
- **Stdio reuse.** The stdio servers use the same host session code as
  config servers, with one `McpHostImpl` for both. The working directory
  is the session directory. The `env` of the entry adds to the
  environment of `airlock acp`. The command uses the `PATH` of
  `airlock acp`. A `HostStopper` stops the processes of the proxy
  without the guest.
- **Remote relay.** The proxy relays http (Streamable HTTP) and sse
  (legacy SSE) servers itself, not through the sandbox network. It uses
  reqwest with redirects off, so headers never leave the origin. It uses
  HTTP/1.1 only. It turns the environment proxy off explicitly with
  `no_proxy()`, because reqwest 0.13 reads `HTTP_PROXY` and
  `HTTPS_PROXY` also with default features off. The relay sends a
  DELETE at close, and a 404 with a session id ends the guest session.
  The legacy SSE endpoint must have the same origin, and no user
  information other than the server URL has (`Url::join` copies the
  user information of the base URL).
- **Sink waits.** A guest client can keep an event stream open and not
  read it. Each guest bridge session has a cancel token, so the removal
  of the session or route ends a waiting sink call. On the host, the
  sink calls of the stdio and remote relays race against "stop + 1 s",
  so `closed` and the end of the session always run.
- **Client backpressure.** The proxy does not read the client while a
  registration runs, and the client reader channel holds 16 lines. A
  full channel stops the stdin read.

## Security

- The bridge is opt-in. Without the flag, no editor command, URL or
  secret reaches the agent.
- The editor is trusted. With the flag, its commands run on the host and
  its URLs are reached from the host, as for config `[mcp]` servers.
  `env` and `headers` stay in `airlock acp`. The guest sees only routes
  and JSON-RPC messages.
- The guest can open only registered routes. The name is checked in the
  proxy, in `airlock start`, in airlockd, and again in each host on
  `open`. The guest cannot change URL, headers or endpoint.
- Relayed traffic does not use the sandbox network rules. The manual
  warns about this.
- The logs show URLs as origin only. The trace log replaces the `env`
  and `headers` values of `mcpServers` with `***` in all client lines.
  The trace log can still show server URLs and arguments.

## Limits

- 16 MiB for each remote message and each SSE line.
- 256 run-time routes in the guest.
- 16 sessions for each server, no idle timeout.
- 10 s for the registration of one request.
- 5 s for the release of a set.
- 1 s for the sink calls of a stopped session.
- 16 client lines in the reader queue.

## Known limits

- After SIGKILL of `airlock acp`, remote sessions get no DELETE. The
  remote server must expire them.
- A stdio server can leave grandchildren (for example `node` under
  `npx`). The leader gets the parent-death signal on Linux, but the
  grandchildren can live on. Most servers exit when their stdin closes.
- The relay does not resume a broken POST event stream.

## Tests

- `acp/tests`: rewrite of all entry kinds, lifecycle (close, load,
  failure, fork, agent exit, registration timeout), a real stdio server
  on the host and a remote DELETE at close. A fake registrar replaces
  the CLI service.
- `mcp/tests`: remote Streamable HTTP and legacy SSE against local axum
  servers.
- `cli/tests/test_mcp_forward.rs`: forwarding and release of grants.
- `airlockd/src/mcp/tests/test_dynamic_routes.rs`: run-time routes.
