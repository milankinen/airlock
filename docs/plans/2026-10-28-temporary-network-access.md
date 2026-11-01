# Temporary network access via admin MCP + claude@2

## Context

Today the user must set up the hooks by hand (`docs/manual/src/tips/experimental-claude-hooks.md`).
When a tool call fails because of a network deny, the failure hook only tells Claude
to "ask the user". Temporary network access is not available.

Goal:
- The claude@2 pack sets up the hooks and the airlock MCP server automatically.
- For a failed Bash call, the failure hook tells the agent to call the `run_with_network`
  MCP tool with the tool id.
- The host shows an OS dialog. If the user approves, the command runs with temporary access
  to the approved hosts only. Access ends when the command ends.

Decisions with the user:
- Grant scope = hosts that the default policy denied during the tool call (found by id)
  + optional hosts that the agent declares. The user sees and approves the full list.
- Approval UI = OS notification with Allow/Deny actions. It shows reason, command, hosts
  and the requested duration. The decision is all or nothing (no per-host selection).
  If no UI is available or the request times out, it is denied.
  - macOS: Notification Center (top-right) through notify-rust. The default backend runs from the
    bare binary. If the spike shows that it is not reliable, use a small helper `.app` with the
    `UNUserNotificationCenter` backend. Fallback chain: `CFUserNotificationDisplayAlert` →
    osascript dialog → deny.
  - Linux: D-Bus notification with actions → deny.
  - Both platforms use the `notify-rust` crate (see Phase 2).
  - The airlock terminal also shows a line for each pending request (in case of Focus/DND or a closed banner).
- The agent gives the requested `duration` (seconds). The notification shows it. The host
  limits it to a hard maximum (`network.grants.max_duration`, default 1 h).
- The MCP server uses the maintained official crate `rmcp` (3.8; features `server`, `macros`,
  `transport-streamable-http-server`), mounted in the admin axum router.
- claude@2 hooks go to `/etc/claude-code/managed-settings.json`. Merge into an existing file
  (or use a `managed-settings.d/` drop-in if Claude Code supports it). Do not overwrite.
  The MCP server goes to user scope with `claude mcp add --scope user`.
- Only claude gets a v2 now.

## High-level design

```
Claude ─hook─▶ admin.airlock/claude/hooks/post-tool-use-failure   (Bash only)
                 snapshot denied hosts for tool_use_id → "call run_with_network(id=…)"
Claude ─MCP──▶ admin.airlock/mcp  tools/call run_with_network{id, reason, cmd, env, cwd, hosts}
  airlockd bridge task (LocalSet):
    NetworkProxy.requestGrant(req) on the NETWORK vsock ──▶ host Approver (OS dialog)
       ◀── GrantResult{ proxy :NetworkProxy (scoped), env, cwd, harden } | denied
    create netns + veth pair (grant src IP 192.168.78.<n>) ; register src IP → scoped proxy
    spawn_collect(cmd) in the netns as sandbox user, capped output
    on exit / disconnect / revoke: kill process group, delete veth + netns,
       unregister, drop scoped proxy → host revokes + closes live grant connections
```

Main points:
- **Grant = a scoped `NetworkProxy` capability.** This is the "token": an unforgeable capnp
  capability, served on the network vsock (bulk traffic stays off the supervisor channel).
  The existing `spawn_poll_loop` and `rpc_connect_tcp` take this type already. Release = drop.
  The host revokes on drop, at `max_duration`, and when the guest connection is lost, and it
  aborts the live connections of the grant. The scoped proxy refuses `socket` targets.
- **One TCP stack, grant chosen by source IP (veth).** The grant netns has a veth peer with a
  per-grant source IP. Packets are routed into the root netns and out through `airlock0`, so the
  existing smoltcp stack gets them. `ConnKey.src` selects the proxy client: the grant map, else
  the default. DNS (10.0.0.1) works through the route. No second stack and no per-grant DNS.
- **Grant-aware policy (host).** The grant scope is checked inside the allow decision, after all
  early returns of `Network::resolve_target`. `deny-always`, public-only/local hosts, port-forward
  hosts, explicit config `deny` rules and middleware denies still win. Grant patterns that are
  refused: `*`, bare wildcards, `localhost`, private and local addresses.
- **Deny log.** `reportDeny` gets `host`, `port` and `reason` (policy-default vs other).
  `DenyReporter` changes from a coalescing `watch` to a bounded mpsc with batching. The guest
  `DenyTracker` keeps a ring of recent denies. Only policy-default denies are grant candidates.
- **Approver (host).** OS notification. It shows sandbox/project (title), agent reason,
  command, hosts and duration. Agent text is cleaned (control characters removed) and
  truncated. Dangerous env keys (`LD_PRELOAD`, `LD_LIBRARY_PATH`, `BASH_ENV`, `ENV`) are
  refused before approval. One open request per sandbox, with a small queue. Late answers
  are ignored, and the notification is removed after the decision or the timeout. The test
  approver is `cfg(test)` only and is never a config key.
- **macOS notifier helper (only if the Phase 2 spike shows that notify-rust's default backend
  does not work from the bare binary).** New bin crate `app/airlock-notifier` (objc2-user-notifications 0.3,
  objc2-app-kit, block2). Takes the request as JSON on stdin. Registers a category with
  Allow/Deny, posts the notification, runs the run loop as an accessory app, writes
  `allow|deny|timeout|unavailable:<why>` as JSON on stdout, and exits. The `airlock` binary
  embeds the helper binary + `Info.plist` (fixed bundle ID, `LSUIElement`, alert style).
  On first use, it writes `AirlockNotifier.app` to a versioned dir under the data dir
  (`VerifiedPath`) and signs it ad hoc with `/usr/bin/codesign -s -`. The permission grant is
  kept per bundle ID, so rebuilds keep it.
- **Admin MCP (guest).** `rmcp` `StreamableHttpService` (stateless, JSON response,
  `with_allowed_hosts(["admin.airlock", ...])`, Origin guard) at `/mcp` on the admin host.
  `#[tool]` handler is `Send` and talks to a `!Send` bridge task (pattern of `mcp/bridge.rs`)
  over mpsc + oneshot.
- **Known limits (documented).** In the grant netns, loopback services (`*.airlock`, guest
  localhost services, `bindHostPort` listeners) cannot be reached. Traffic through daemons that
  run in the root netns (for example dockerd) does not get the grant. A root agent can enter the
  netns or fake the source IP while the grant is live; the scope limits the damage (the user
  accepted this).

## Detailed plan

Order: 1a first (schema). Then 1, 2 and 3 run in parallel. 4 needs 1 and 3. 5 needs 4.

### Phase 1 — Host grant engine (airlock-cli, airlock-common) [opus]
1a. Schema:
   - `supervisor.capnp` `reportDeny @4`: add `host :Text, port :UInt16, reason :DenyReason`.
   - `network.capnp`: `NetworkProxy.requestGrant @1 (req :GrantRequest) -> (result :GrantResult)`.
     `GrantRequest{ id, reason, cmd, cwd, env :List(Text), hosts :List(Text), durationSecs :UInt32 }`.
     `GrantResult union { granted :Granted{ proxy :NetworkProxy, hosts, env, cwd, harden, expiresMs }, denied :Text }`.
1b. `rpc/network.rs`: share `Rc<Network>` between the default proxy server and the grant
   servers. `network/grant.rs`: `ScopedProxy` (scope from `rules.rs` `parse_pattern`,
   expiry timer, abort handles of live connections; abort all of them on `Drop`, at expiry,
   or at disconnect). `requestGrant` validates the patterns, calls the `Approver` (phase 2) and
   builds the env/cwd/harden from the same source as `cli_server` exec (`boot.rs:251`,
   `rpc.rs:337` spawn creds).
1c. `network/mod.rs` `resolve_target` / `is_allowed`: take an optional grant scope, as described in the design.
1d. `deny_reporter.rs`: bounded mpsc with batching. `report(host, port, reason)` at
   `network/server.rs:68,81,272,289` and `network/http/middleware.rs`.
1e. Config `[network.grants]`: `enabled` (default true), `approval_timeout` (120 s),
   `max_duration` (hard limit for the agent's `duration`, 1 h). Add it to `config_values.rs`. `packs/install/phase.rs` `isolate()`
   disables grants.
1f. Test `network/tests/test_temporary_grant.rs` with a real upstream and a fake approver. Check that
   the granted host is allowed, other hosts are denied, config deny wins, `*` is refused, and drop closes the live conn.

### Phase 2 — Host approver (airlock-cli, airlock-notifier) [opus for 2b, sonnet else]
2a. `approval/mod.rs`: `Approver` trait (`async fn approve(&Request) -> Decision`),
   per-sandbox queue, timeout → deny, text cleanup, a terminal line for each pending request.
2b. One notification API for all platforms: `notify-rust` 4.18 (MIT/Apache, maintained).
   Actions + wait for the response. Linux: zbus D-Bus backend (`z-with-tokio`).
   macOS has two backends:
   - default: deprecated `NSUserNotification` (via mac-notification-sys). It works from a bare
     CLI because it borrows a bundle ID (default `com.apple.Terminal`). Risks: deprecated API,
     reports of silently dropped notifications on macOS 15+, buttons only in Alerts style.
   - `preview-macos-un`: `UNUserNotificationCenter`, needs a bundled and signed app
     (`check_bundle()`), so it needs the helper `.app` below.
   Spike first on current macOS: (1) the default backend from the bare `airlock` binary, (2) the
   helper `.app` with `preview-macos-un`. If (1) shows Allow/Deny and returns the answer
   reliably, use it and drop the helper. Otherwise use (2). Then (only for (2)):
   - `app/airlock-notifier` crate. `Info.plist` and the embed step go in `build.rs` of airlock-cli
     (macOS only). Add a mise task to build it.
   - `approval/macos.rs`: install the bundle (versioned, `VerifiedPath`) and sign it ad hoc.
     Run it, kill it at the timeout.
   - Fallbacks: `CFUserNotificationDisplayAlert` (objc2-core-foundation), then osascript
     `display dialog` with `on run argv`.
   - Release: the `sign-macos` job also signs the embedded helper. Check that the archive count
     stays the same.
2c. Linux: notify-rust actions `allow`/`deny` + `wait_for_action` on a blocking thread with the
   approval timeout. If the server has no `actions` capability (`get_capabilities`), or D-Bus is
   missing, deny with a clear message.
2d. Texts in `messages.rs` `pub mod grants`. Log each decision and send a monitor event.

### Phase 3 — Guest grant netns + source routing (airlockd) [opus]
3a. `net/tcp_proxy.rs`: a shared `GrantRoutes` (`Rc<RefCell<HashMap<IpAddr, network_proxy::Client>>>`).
   `relay_agent` picks the client by `ConnKey.src`.
3b. `net/grant_ns.rs` (Linux; stubs in `net/mod.rs` for other OSes):
   - Use a helper std thread for `unshare(CLONE_NEWNET)`. Keep the fd of `/proc/thread-self/ns/net`.
   - Create the veth pair with `ip`. Move the peer into the netns. Grant side `192.168.78.<2n>/31`
     with a default route through the root side. Bring up `lo` in the netns. Set the sysctls
     (`rp_filter=0`, `route_localnet` on both netns as needed).
   - In the root netns, route the grant return traffic out through the veth.
   - Allocate grant IPs from a small pool.
   - The returned handle unregisters the route, deletes the veth and closes the ns fd on drop.
3c. `process.rs`: `spawn_collect(spec, netns_fd)` modeled on `spawn_daemon`.
   - stdin is `/dev/null`. stdout and stderr go to capped tail buffers (64 KiB each).
   - Use `process_group(0)`. In pre-exec, `setns(fd, CLONE_NEWNET)` comes before the mount ns join, harden and setuid.
   - Kill the process group when the wait ends or is cancelled.
3d. Cover the netns setup in bats (needs a real VM).

### Phase 4 — Admin MCP + hooks (airlockd) [opus]
4a. `admin/deny_tracker.rs`: a ring of (epoch, host, port, reason), 256 entries. Keep `last()`.
   `rpc.rs` `report_deny` fills it.
4b. `admin/tool_tracker.rs`: store start and end times. `take` is idempotent in a short window,
   so duplicate v1 + v2 hooks still work. `admin/pending.rs` maps tool id → (start, end).
4c. Failure hook: only for `tool_name == "Bash"`. Context (STE) names the denied hosts and says:
   "To run the command with network access, call the tool `run_with_network` of MCP server
   `airlock` with `id: "<tool_use_id>"`, the command, and a short reason. The user must approve
   the request." Other tools keep the current text.
4d. Add `rmcp` to `[workspace.dependencies]` and use it in airlockd only. Check the musl build
   and the binary size. `admin/mcp/{mod,tools,bridge}.rs`. Tool input:
   `{ id, reason, cmd, duration, env?, cwd?, hosts? }` (`duration` in seconds, required).
   `run_with_network`:
   - Candidate hosts = policy-default denies in the id's time window (looked up at call time,
     because deny reports are async) + declared hosts.
   - Call `requestGrant` through the bridge.
   - With the result: netns, register the route, `spawn_collect`, then teardown.
   - If the reply oneshot is closed (client disconnect) or the grant expires, kill and tear down.
   - The result is JSON text: exit code, stdout/stderr tails, granted hosts, the limits note.
     Errors use `isError: true`.
4e. Tests: `admin/tests/test_admin_mcp.rs` (protocol, and a fake `NetworkProxy` server that
   grants or denies). Update `test_claude_hooks.rs`.

### Phase 5 — claude@2 pack + docs [sonnet]
5a. Check in the Claude Code docs:
   - the managed-settings path, or a `managed-settings.d/` drop-in
   - that hooks merge across sources
   - `http` hooks in managed settings
   - user-scope MCP in `~/.claude.json`
   - the MCP tool-call timeout (must be longer than approval + run time; set `MCP_TOOL_TIMEOUT` in config.lua if needed)
5b. `packs/claude@2/`: `pack.toml` (same label and kind as claude@1), `config.lua`, `setup.sh`.
   Inline all logic in `setup.sh`.
   - `"$_claude" mcp add --scope user --transport http airlock http://admin.airlock/mcp`
     (run as the run user, so that `~/.claude.json` keeps its owner).
   - `pkg_install jq`. Merge the three http hooks and `permissions.allow +=
     mcp__airlock__run_with_network` into the managed settings. Skip entries that are already
     there. Write in place, so that the owner and mode stay the same.
   - Update the `builtin_packs_load` test (`builtin.rs:466`).
5c. Docs (STE, `/asd-ste100`, lint): `packs/agents/claude-code.md` (v2),
   `configuration/network.md` section "Temporary access" (flow, dialog, config, limits),
   and replace `tips/experimental-claude-hooks.md` with a short v1 note plus a migration hint
   (remove the manual hooks). Update `SUMMARY.md`.
5d. Add `docs/log/2026-10-28-temporary-network-access.md`.

### Verification
- `mise run test`, `mise run bats`, `mise format`, clippy (VerifiedPath / disallowed rules).
- Bats test: a deny-by-default sandbox and a test-build approver that allows. `curl` the
  admin MCP with `run_with_network{cmd:"curl https://example.com"}` → exit 0. The same curl
  outside the grant → denied. After the call ends, the grant IP route is gone.
- Manual test in `.tmp/test-grants` with `claude@2` on macOS. Ask Claude to fetch a denied URL,
  approve in the dialog, and check that the command succeeds and that access ends after it.

### Execution
One sub-agent per phase, with the model tier shown at each phase. The critic for each phase uses a
different tier. Make a WIP commit after each green phase.
