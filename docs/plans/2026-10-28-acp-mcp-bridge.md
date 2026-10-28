# Plan: bridge editor MCP servers from `airlock acp` into the sandbox

## Context

Editors send MCP servers in `mcpServers` of ACP `session/new` (required),
`session/load` (required), `session/resume` and `session/fork` (optional).
Today `airlock acp` forwards stdio/http/sse entries unchanged, so host
commands (`npx`, editor-bundled node) and `localhost` URLs fail in the VM,
and `env`/`headers` secrets go into the VM. Goal (user decisions):

1. New flag `airlock acp --proxy-mcp-servers`. Without it, the proxy
   removes all `mcpServers` entries (the agent gets `[]`, one info line in
   acp.log). With it, the proxy bridges the supported entries (stdio with
   no `type` or `"type":"stdio"`, `http`, `sse`) and drops the others
   (`acp`, unknown, invalid) with a warning in acp.log.
2. Each bridged entry goes to the agent as
   `{"type":"http","name":<orig>,"url":"http://acp.mcp.airlock/<key>/<name>","headers":[]}`.
   `<key>` is a random proxy key for one ACP session, so sessions of one
   proxy and of other proxies never clash. The `airlock acp` process
   implements the capnp `McpHost`. It passes the capability over the CLI
   capnp service to `airlock start`, which forwards it to airlockd with a
   runtime route. Stdio servers reuse the existing host session code. The
   acp process relays http/sse itself, not through the sandbox network
   stack.
3. The bridges of a session end when the client closes, replaces or fails
   that session, and all bridges end with the `airlock acp` process. They
   are cleared on the host (processes stopped, remote sessions deleted)
   and in the sandbox (route removed, guest sessions ended).

## High-level design

| Where | New or changed | Role |
|---|---|---|
| `supervisor.capnp` | `addMcpServer @10 (session :Text, name :Text, host :McpHost) -> (grant :McpGrant)`, `interface McpGrant { release @0 () }` | Runtime route at `http://acp.mcp.airlock/<session>/<name>`. `release` (awaited) or dropping the grant removes it (same pattern as `bindHostPort`/`ListenHandle`). |
| `cli.capnp` | `addMcpServer @3 (session, name, host) -> (grant)` | The start process checks `session` and `name` (each 1-64 chars of `[A-Za-z0-9_-]`) and forwards host cap and grant unwrapped. Client disconnect releases the grant. |
| airlockd `mcp::Bridge` | Owns `servers: route -> {host, id}` and the sessions. Started in `main` before boot, `add_boot` at boot. The route check moves into the bridge task (single owner); `McpState.names` goes away and `index` asks the bridge | One bridge with a host per route. Config servers stay at `mcp.airlock/<name>` (permanent). Runtime servers live under the new host name `acp.mcp.airlock` at `/<session>/<name>` (max 256). Unregister ends all sessions of the route, also opens in flight (`process: None`), then 404. |
| airlock-cli `mcp::ServerSpec` | `pub(crate)`, `transport: Stdio{command,args,env} \| Remote(RemoteSpec)` | One `McpHostImpl` (limits, eviction) for config and ACP servers. |
| airlock-cli `mcp::HostStopper` | `close(reason)` refuses new opens and stops live sessions | The acp process stops its processes and remote sessions without the guest. |
| airlock-cli `mcp::remote` (new) | Streamable HTTP client, legacy SSE client, SSE parser; `RemoteSession` implements `McpSession` | Relays MCP messages to the remote URL with the editor headers. reqwest (already a dependency, rustls with the host trust store). Own client: connect timeout, no total timeout, **no redirects** (headers never leave the origin), HTTP/1.1 (no `http2` feature), no env proxy (off with `default-features = false`; verify for 0.13). |
| airlock-cli `acp/mcp.rs` (new) | `McpServers` registry: parse, sanitize names, per-session server sets keyed by proxy key, pending request map, ACP session id -> proxy key | Rewrites `mcpServers` and ties registrations to ACP sessions. |
| `AgentProcess` | `mcp: Option<cli_service::Client>` | The agent's own CLI connection is the registrar. Tests use a fake registrar. |

Flow:

```
editor --session/new{cwd, mcpServers}--> airlock acp (--proxy-mcp-servers)
  acp/mcp.rs: new proxy key K; for each entry: parse -> McpHostImpl{spec,
  cwd = host session cwd} -> cap H
    cli.addMcpServer(K, name, H) --unix--> airlock start (check)
      supervisor.addMcpServer(K, name, H) --vsock--> airlockd Bridge.Register
    <-- McpGrant
  rewrite entry -> http://acp.mcp.airlock/K/<name>; remember pending id -> K
  --session/new--> agent (VM)
agent POST http://acp.mcp.airlock/K/x -> airlockd -> H.open over capnp
  -> acp process: stdio session::start | remote::start (http/sse)
agent result{sessionId S} -> registry maps S -> K
release of K (session/close OK, failed request, replace on load/resume):
  HostStopper.close + McpGrant.release -> airlockd Unregister
acp process dies: unix connection closes -> grants released -> Unregister;
host processes die with the acp process (kill_on_drop, process group).
```

## Decisions

- Flag off (default): every `mcpServers` list becomes `[]`, in restrict
  and pass mode. The proxy `initialize` result advertises no
  `mcpCapabilities`. Flag on: it advertises `http` and `sse` (the proxy
  terminates both). The agent needs only `http`.
- Proxy key: 16 random hex chars from the OS random source (reuse the
  existing random helper of airlock-cli if one exists). Session mapping:
  - `session/new`, `session/fork`: new key. On an OK response, map
    `result.sessionId` to the key. On an error, release the key. A fork
    leaves the source session as it is.
  - `session/load`, `session/resume` of a known ACP session: reuse its
    key, so its URLs stay the same. First release the old set of the key
    and await the grant releases, then register the new set. On an error,
    release the key and forget the session.
  - `session/load`, `session/resume` of an unknown session: new key, map
    on an OK response.
  - Absent `mcpServers` on resume/fork = `[]`.
  - `session/close` (unstable): release the set after an OK response.
  - Pending map keyed by the canonical JSON text of the request id. The
    response hook runs in restrict and pass mode. Agent exit, client end
    and failed start release all keys.
- Names: `<name>` is the entry name with chars outside `[A-Za-z0-9_-]`
  replaced by `_`, cut to 64 bytes, empty -> `mcp`, unique within the
  session with suffix `-2`, `-3`. The agent sees the original name in the
  entry.
- Registration does not block the proxy loop. It runs as a future in the
  main `select!` loop (like the agent start). The session request and all
  later client lines wait in order until it completes. All servers of one
  request register in parallel with one shared 10 s limit. A server that
  fails or times out is dropped with a warning.
- Agent without `mcpCapabilities.http`: drop all bridged entries, one
  warning. The session still runs.
- Pass mode with the flag: bridges stdio/http/sse too, keeps acp/unknown
  entries as is. Restrict mode drops acp/unknown.
- ACP server limits: no idle timeout, `max_sessions = 16` for each server.
- Env: ACP `env` on top of the acp process env. Command resolves through
  the acp process `PATH` (Zed sends `npx`). Stdio cwd = host session cwd
  (`VerifiedPath::new_follow`, client-chosen entry point), resolved for
  each request.
- Remote relay: concurrent POSTs after initialize (bounded 64), 16 MiB
  cap for each message and each SSE line, GET stream with max 3
  reconnects, DELETE at close (best effort; at proxy exit awaited inside
  the existing shutdown grace, a second signal skips it), 404 with a
  session id = expired -> close the guest session. `Mcp-Session-Id` must
  be a visible ASCII header value. Legacy SSE endpoint: resolved against
  the URL, same scheme/host/port, no userinfo.
- Logs: never log env or header values. Log URLs as origin only. Strip
  control characters from names in logs.

## Phases (each ends with format, lint, test; P2/P4/P5 also bats:cli)

Order: P0 first. Then P1, P2, P3a in parallel. P3b after P3a. P4 after P0,
P3a, P3b (uses a fake registrar). P5 last.

- **P0 Schema**: `supervisor.capnp` `addMcpServer @10` + `McpGrant`
  (`release @0`); `cli.capnp` `addMcpServer @3`. Comments updated.
  Generated `Server` traits default new methods to "unimplemented", so
  both crates build. Confirm the build script picks up the schemas.
- **P1 Guest dynamic routes** (airlockd `mcp/{mod,bridge,routes}.rs`,
  `rpc.rs` incl. `rpc::serve` signature and callers, `main.rs`,
  `admin/server.rs` (route `acp.mcp.airlock` to the same MCP router),
  `test_cfg/mcp.rs`): bridge starts in `main` before boot and owns the
  route -> host map; `add_boot(host, names)`; guest `McpGrantImpl`
  (`release` unregisters and returns after sessions end; drop unregisters
  by (route, id)); router always exists, with `/{name}` for
  `mcp.airlock` and `/{session}/{name}` for `acp.mcp.airlock`; origin
  allow list gets `acp.mcp.airlock`; `GET /` lists the routes of its host
  name; route check inside the bridge task; 404 for unknown routes; max
  256 runtime servers; `add_mcp_server` gated like `bind_host_port`. New
  `mcp/tests/test_dynamic_routes.rs`: runtime server serves until grant
  release; duplicate (session, name) refused; config and runtime hosts
  separate; open in flight while the grant is released ends cleanly and
  stops the host session.
- **P2 Start process forwarding** (`cli_server.rs`, `rpc/supervisor.rs`
  (P2 owns this file), `messages.rs`): `CliServiceImpl::add_mcp_server`
  checks session and name and forwards host cap and grant unwrapped. New
  `cli/tests/test_mcp_forward.rs` with a fake supervisor in `test_cfg`:
  server reaches guest; explicit `release` and client disconnect both
  release the grant; bad session or name refused, as asserts in one test.
- **P3a mcp refactor** (`mcp/{mod,session}.rs`, no behavior change):
  `pub(crate) ServerSpec` with `Transport` that has only `Stdio` here,
  `McpHostImpl::new`, `HostStopper`, shared `SessionState`. Module docs
  updated (not config-only). Existing `test_stdio_session.rs` stays
  green; add `closed_host_stops_sessions_and_refuses_open`.
- **P3b Remote relay** (`mcp/remote/{mod,streamable,legacy_sse,sse}.rs`,
  adds `Transport::Remote` to `mcp/mod.rs`, `messages.rs` `mcp` texts,
  `test_cfg/mcp_remote.rs` axum fake servers on `airlock-test-utils`
  `serve`): reqwest client (no redirects), Streamable HTTP state machine
  (init gate, session id, JSON/SSE responses, GET stream, DELETE,
  expiry), legacy SSE (endpoint event, same origin, no userinfo), SSE
  parser with one inline table test (multi-line data, CRLF split,
  id/retry, BOM, size cap). Tests: `test_remote_streamable.rs` (headers
  and session id on each POST, SSE responses and GET stream, DELETE at
  release, expiry, HTTP error -> JSON-RPC error, redirect not followed),
  `test_remote_legacy_sse.rs` (endpoint flow, other origin refused,
  stream end closes the sink).
- **P4 ACP integration** (`cli/cmd_acp.rs` flag, `acp/{mcp,mod,proxy,
  filter,sandbox}.rs`, `messages.rs` `acp` texts, `test_cfg/acp.rs`):
  `--proxy-mcp-servers` in `ProxyConfig`; flag off removes all entries;
  registry; registration future in the `select!` loop with an ordered
  hold queue; rewrite after the cwd and feature checks, with the host cwd
  resolved per request; response hook in both modes; release on close /
  replace / error / agent exit / client end / failed start. Remove
  `remove_client_mcp_servers` and the http/sse refusal; the initialize
  result depends on the flag. Module docs updated. Fake agent returns
  `sessionId` (opt-in env); `FakeRegistrar` fake `CliService` (other
  methods error, can be made slow). Tests: `test_mcp_rewrite.rs` (flag
  off gives `[]`; all entry kinds; name sanitizer and duplicates; two
  sessions with the same server get separate keys; agent without http;
  pass mode), `test_mcp_lifecycle.rs` (close, load of a known session
  keeps the key and replaces the set, failed request, fork keeps the
  source, agent exit, registration timeout keeps the proxy responsive),
  `test_mcp_stdio_bridge.rs` (real shell server on host: cwd, env,
  stopped at client close), `test_mcp_remote_bridge.rs` (DELETE at
  client close). Update `test_deferred_start.rs`, `test_pass_mode.rs`,
  filter unit test.
- **P5 Docs + VM test**: manual pages with the `/asd-ste100` skill and
  its linter. `tips/editor-acp.md`: section "Editor MCP servers"
  (`--proxy-mcp-servers`, what is bridged and dropped, warning: tools act
  on the host, no sandbox network rules), options table, Restrictions;
  `technical/rpc.md`, `technical/networking.md`, `configuration/mcp.md`
  notes (`acp.mcp.airlock`); `docs/log/2026-10-28-acp-mcp-bridge.md`;
  optional `tests/vm/acp_mcp.bats`.

## Security

- The bridge is opt-in (`--proxy-mcp-servers`). Without it, no editor
  command, URL or secret reaches the agent.
- The editor is trusted: with the flag, its commands run on the host and
  its URLs are reached from the host, like config `[mcp]` servers. Env
  and headers stay in the acp process. The guest sees only routes and
  JSON-RPC messages.
- The guest can open only registered routes. Session and name are checked
  in three places (acp sanitizer, start process, guest), and each
  `McpHostImpl` checks its own name on `open`.
- Relayed traffic does not use sandbox network rules (user decision). The
  guest cannot change URL, headers or endpoint.
- Bounded resources: runtime routes, sessions per server, in-flight
  POSTs, message size. Everything ends at grant release or acp process
  exit.

## Verification

- Per phase: `mise run format`, `mise run lint`, `mise run test`, and
  `mise run bats:cli` for P2/P4/P5.
- E2E unit-level: the tests listed per phase (real processes, real local
  axum MCP servers, fake only the sandbox edge).
- Manual: `airlock start` in `.tmp/test-acp-mcp`, editor-like scripted
  stdin to `mise airlock -- acp --proxy-mcp-servers --log-level debug --
  <agent>` with a stdio and an http server; check acp.log,
  `curl http://acp.mcp.airlock/` in the VM, and that routes vanish after
  `session/close` and after the proxy exits.
- Optional VM bats `tests/vm/acp_mcp.bats` where KVM is available.

## Risks

- A SIGKILL of the acp process leaves remote sessions without DELETE (the
  server expires them). The stdio server leader gets the parent death
  signal (Linux), but its children (e.g. `node` under `npx`) can live on.
  Most servers also exit when their stdin closes, which happens when the
  acp process dies. Same as config `[mcp]` servers today.
- Concurrent POSTs can reorder notifications at the remote (spec allows).
- Registration waits up to 10 s if `airlock start` is stuck.
