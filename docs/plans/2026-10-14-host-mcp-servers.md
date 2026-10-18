# Plan: host stdio MCP servers as HTTP endpoints in the guest

## Context

Users want MCP servers that must run on the host (e.g. `mise mcp`, tools that
need host state) to be reachable by agents inside the sandbox. A config entry
`[mcp.<name>]` describes a stdio MCP server. Airlock exposes it inside the
guest as an MCP Streamable HTTP endpoint at `http://mcp.airlock/<name>`.

```toml
[mcp.mise]
enabled = true            # default
command = "mise"
args = ["mcp"]
env.SOMETHING = "${SAME_SUBST_AS_IN_ELSEWHERE:}"
env.OTHER = "hard-coded"
```

Crate research: no maintained Rust crate does raw stdio→Streamable-HTTP
passthrough. `rmcp` 3.5 (official SDK) can build a proxy only with typed
handlers: it drops unknown methods and does not fully forward notifications
or server→client requests (sampling, elicitation). The existing proxy crates
(`mcp-streamable-proxy`, `stdio2http`) use rmcp, have little adoption, and pull
in heavy dependencies. **Decision: write our own small Streamable HTTP bridge
(JSON-RPC passthrough, about 300 lines).** It uses only crates we already have
(axum in airlockd, tokio, serde_json).

## High-level design

```
guest app ──HTTP──> mcp.airlock (127.0.0.1:80, airlockd)
                     │ Host-header vhost dispatch
                     ├─ admin.airlock → existing admin router
                     └─ mcp.airlock/<name> → Streamable HTTP bridge
                                   │ capnp: McpHost.open(name, sink) → McpSession
                                   ▼
                     host (airlock-cli): spawn `command args` with env,
                     stdin/stdout line relay, one child per MCP session
```

- **Virtual hosts (guest):** guest DNS reserves `mcp.airlock` → 127.0.0.1, the
  same as `admin.airlock`. The server on 127.0.0.1:80 picks a router by the
  `Host` header (without the port). It returns 404 for an unknown host. This
  is a small tower/axum layer, not a crate.
- **Streamable HTTP lives in the guest.** The host only relays JSON-RPC lines.
  Thus the capnp surface is small and the HTTP details stay in one place.
- **One host child per MCP session.** A stdio server expects one client.
  Thus we need no JSON-RPC id rewriting, and sessions are isolated.
- **Capability-based access:** the host passes an `McpHost` capability in
  `boot` (null when no MCP servers are enabled), the same model as
  Browser/Clipboard. The guest can open only names that the host config
  holds. The host checks the name on each `open`.
- **Security:** the existing config-change prompt protects `[mcp.*]`, because
  the field is part of `ConfigValues` (pack entries included). Env templates
  are resolved on the host at sandbox start and never go to the guest. The
  install boot (`packs/install/phase.rs` `isolate`) disables MCP.
- **Lifecycle:** children use `tokio::process` with `kill_on_drop` and
  `process_group(0)`, plus PDEATHSIG on Linux (reuse `tie_to_airlock` logic
  from `vm/cloud_hypervisor.rs`). A child stops when its capnp `McpSession`
  is released (HTTP DELETE, idle guest session, VM end). The host drains the
  child's stderr to the tracing log.

## Detailed plan

### Phase 1 — Config (`airlock-cli`)

- `config/config_values.rs`: add `pub mcp: BTreeMap<String, McpServer>` on
  `ConfigValues` with `#[serde(skip_serializing_if = "BTreeMap::is_empty")]`.
  `McpServer { enabled: bool (default_t = true), command: String,
  args: Vec<String> (default), env: BTreeMap<String,String> (default) }`.
  `impl WellKnown` with `de::Nested`, as for `Daemon`.
- `validate()`: add `validate_mcp`. Name must match `[A-Za-z0-9_-]+` (one URL
  path segment). `command` must not be empty. Texts go in
  `messages::config`.
- `packs/install/phase.rs` `isolate()`: clear `mcp`.
- Tests: extend the config tests (TOML parse with defaults, invalid name,
  empty command, layered `args` concat). `airlock config` export and the
  config-change check pick up the field automatically. Add one test that a
  changed `[mcp.x].command` triggers the change check.

### Phase 2 — Capnp schema (`airlock-common/schema/supervisor.capnp`)

```capnp
# boot @0: add field
mcp :McpConfig,

struct McpConfig {
  # Null unless at least one MCP server is enabled.
  host  @0 :McpHost;
  # Names of the enabled servers, for the index page and early 404.
  names @1 :List(Text);
}

interface McpHost {
  # Start a new instance of server `name`. `sink` gets each line that the
  # server writes to stdout and `closed` when the process exits.
  open @0 (name :Text, sink :McpSink) -> (session :McpSession);
}
interface McpSession {
  # Write one JSON-RPC message (one line, no newline) to the server stdin.
  send @0 (message :Text) -> ();
}
interface McpSink {
  message @0 (message :Text) -> ();
  closed  @1 (reason :Text) -> ();
}
```

Releasing `McpSession` stops the child.

### Phase 3 — Host side (`airlock-cli`)

- New module `mcp/` (or `rpc/mcp.rs` plus `mcp/process.rs`):
  - `McpServers::resolve(config, vault)`: for each enabled entry, resolve the
    env with `Vault::subst` (copy the `daemon.rs:39-48` pattern and error
    text) at sandbox start. Fail the start on an undefined variable.
  - `McpHostImpl` (capnp server): `open` checks the name, spawns the child
    (`tokio::process::Command`, project dir as cwd, the resolved env on top
    of the host env, piped stdin/stdout/stderr, `kill_on_drop(true)`). It
    starts tasks: a stdout line reader → `sink.message` (keep the order, await
    each call); an stderr drain → `tracing`; and a wait task → `sink.closed`.
    It returns `McpSessionImpl` that owns the child and the stdin writer.
    `Drop` kills the child.
  - Limit the line size (e.g. 16 MiB) so that a bad server cannot make the
    host buffer without bounds.
- Wire into `rpc/supervisor.rs` boot params and `sandbox/interactive.rs`
  (build after the vault, before boot). Exec-only flows reuse the same VM, so
  nothing more is needed.
- Messages in `messages.rs` (`pub mod mcp`).
- Test: `mcp/tests/test_stdio_session.rs`: real child (a small
  `sh`/python-free echo script, e.g. `sh -c 'while read l; do echo "$l"; done'`),
  through `rpc_loopback`. Check message order, env substitution, `closed` on
  exit, and that the child dies when the session is released.

### Phase 4 — Guest side (`airlockd`)

- `net/dns.rs`: reserve `mcp.airlock` → loopback.
- `admin/server.rs`: rename the concept to "local HTTP server" only if
  needed. Keep `start()`, but build `vhost_router(admin_router, mcp_router)`:
  axum `Router` fallback service that matches the `Host` header (lowercase,
  port removed). Unknown host → 404. Keep the `router()` function for tests.
- New module `mcp/` (`mcp/mod.rs`, `mcp/bridge.rs`, `mcp/routes.rs`):
  Streamable HTTP (MCP spec 2025-06-18), passthrough of `serde_json::Value`:
  - `POST /<name>`:
    - No `Mcp-Session-Id` and body is `initialize` → `McpHost.open`, create a
      session with a random id (UUID v4 or 128-bit hex), return the id in the
      `Mcp-Session-Id` header.
    - A missing or unknown session id on another message → 400 / 404 (per spec).
    - Body is a request (has `id` and `method`): register a pending entry for
      the id, send the line, and answer with `text/event-stream`. The stream
      sends each server message that arrives while the request is pending
      (notifications, server→client requests) and ends after the matching
      response. Use `application/json` when the client does not accept SSE.
    - Body is a notification or a client response → send, return 202.
    - A JSON array (old batch) → 400.
  - `GET /<name>`: SSE stream for server messages when no POST stream is
    open. At most one per session (a second → 409).
  - `DELETE /<name>`: drop the session (this releases the capnp session and
    the host kills the child) → 200.
  - `GET /` on `mcp.airlock`: plain-text list of names (for discovery).
  - Routing of child messages: response → its pending POST; other → the
    newest open POST stream, else the GET stream, else a small bounded queue
    (drop the oldest on overflow, log a warning).
  - When the child closes: end all streams. Later requests get 404, so the
    client starts a new session.
  - Validate the `Origin` header: if present, it must be for `mcp.airlock`
    (DNS-rebinding guard from the spec).
- `main.rs`: pass `cfg.mcp` into the server state.
- Test: `mcp/tests/test_streamable_http.rs`: real axum server and a fake
  `McpHost` capnp server (an in-process echo MCP that answers `initialize`,
  `tools/list`, and sends one notification and one server request). Use
  `rpc_loopback`. Cover: init plus session header, SSE request with an
  interleaved notification, a 202 notification, a GET stream, DELETE, an
  unknown session → 404, and vhost dispatch (admin vs mcp vs unknown host).

### Phase 5 — End-to-end, docs, log

- bats `tests/vm/mcp.bats`: config with `[mcp.echo]` that runs a tiny
  stdio MCP script on the host. Inside the VM, `curl` initialize plus
  `tools/list` to `http://mcp.airlock/echo`. Add `tests/cli/config.bats`
  cases for validation errors.
- Manual: new `docs/manual/src/configuration/mcp.md` (keys, endpoint URL,
  security note: the server runs on the host with your user's rights, and
  an example Claude Code `.mcp.json` with `"type": "http"`). Add it to
  `SUMMARY.md` and `configuration.md`. Update `technical.md` and
  `technical/networking.md` for the reserved names. Use the `/asd-ste100`
  skill and the lint.
- Dev log `docs/log/2026-10-14-host-mcp-servers.md`.

## Fixes from plan review (they override the text above)

1. **airlockd `!Send`:** axum handlers must be `Send`, and capnp clients are
   `Rc`. Only one `spawn_local` bridge task holds the capnp objects (`McpHost`,
   each `McpSession`, the `McpSink` impl). Handlers talk to it through
   `tokio::sync::{mpsc, oneshot}`. The SSE bodies come from `mpsc` receivers
   (`futures::stream`). Add the `sync` feature to airlockd tokio explicitly.
2. **Session id:** 128-bit hex from a CSPRNG (add `rand` workspace dep to
   airlockd, or `getrandom`). Not "UUID".
3. **Vhost back-compat:** only `Host: mcp.airlock` goes to MCP. All other
   hosts (`admin.airlock`, `localhost`, IPs) go to the admin router as
   before.
4. **Host wiring:** add `mcp: Option<McpHostImpl>` to `BootSpec`
   (`sandbox/boot.rs`) and to the boot params in `rpc/supervisor.rs`, next to
   `browser`/`clipboard`. `packs/install/setup.rs` passes `None`. Guest:
   add `mcp` to the boot config in `airlockd/src/rpc.rs` and pass it into
   `admin::start`. Exec flows (`airlock exec`, `cli/sandbox_client.rs`) reuse
   the running VM: no change.
5. **Process tie:** move `tie_to_airlock` from `vm/cloud_hypervisor.rs` to a
   shared helper (Linux: `process_group(0)` + PDEATHSIG; macOS: process group
   only). On drop, kill the process group (`killpg` SIGKILL), so grandchildren
   (e.g. what `mise mcp` starts) also stop.
6. **Session release:** the bridge task is the only owner of each
   `McpSession`. The session ends on DELETE, on child exit, or at VM end. No
   idle timeout in this iteration (MCP clients keep sessions open for a long
   time).
7. **Backpressure:** the host awaits each `sink.message` (keeps the order). A
   slow guest thus slows the child stdout, which is acceptable. The guest
   queue for unrouted messages is bounded (256, drop the oldest).
8. **Spec 2025-06-18:**
   - POST: `Accept` must list `application/json` and `text/event-stream`,
     else 406.
   - `MCP-Protocol-Version`: pass it through. Do not validate it (the child
     validates the version during `initialize`).
   - A POST with only a notification or a response → 202, empty body.
   - DELETE or another request with an unknown session → 404.
   - SSE: send a keepalive comment every 15 s. No `Last-Event-ID`
     resumability (documented).
   - Origin: a missing header is OK. Allowed values: `mcp.airlock`,
     `localhost`, `127.0.0.1` (any scheme and port).
   - Unsolicited messages go to the newest open POST stream. The child gives
     no related-request info, so this is a best-effort match (document it in
     a code comment).
9. **Config tests:** update the tests that list sections
   (`test_config_sections.rs`, `test_config_export.rs`,
   `test_presets_list.rs`), if they list them. `command` stays one string and
   `args` a list, as the user gave.

## Execution and agents

Phase 1 and Phase 2 first (small, sequential: the schema is needed by both
sides). After that, Phase 3 (host) and Phase 4 (guest) run in parallel, with
one opus implementer each. Phase 5 runs after them (a sonnet agent for docs).
Each phase gets a critic agent from a different tier. The gate for each phase:
`mise format`, `mise run lint` (clippy), `mise run test` pass. WIP commit
after each phase.

## Verification

1. `mise run test` (all new unit and e2e tests), `mise run bats:cli`.
2. `mise run bats:vm` (`mcp.bats`) on a KVM host.
3. Manual: `.tmp/test-mcp/airlock.toml` with `[mcp.mise] command="mise"
   args=["mcp"]`, `mise airlock -- start -- curl ...`, and then Claude Code
   in the sandbox with `claude mcp add --transport http mise http://mcp.airlock/mise`,
   and check that the tools are listed.

## Open points (defaults chosen)

- `args` concatenates across config layers (as for daemon `command`).
  Accepted.
- No monitor-TUI events for MCP traffic in this iteration.
- No legacy SSE transport (`/sse` + `/messages`).
