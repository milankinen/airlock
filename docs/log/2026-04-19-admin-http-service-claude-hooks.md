# Admin HTTP service + Claude Code hook endpoints

The deny-backpropagation work shipped a single-purpose axum server on
port 1337 with a `GET /last_deny` endpoint. Useful, but one endpoint
doesn't justify a service, and polling "was there a deny recently" from
shell glue was the wrong abstraction — Claude Code already has a native
hook protocol that fires on tool lifecycle events and can inject context
back into the agent. This commit promotes the endpoint into a proper
admin surface and wires three Claude hook routes on top of it.

## Shape

### Name + reachability

- New hostname `admin.airlock` is reserved in the guest DNS server and
  resolves to `127.0.0.1`.
- The admin server binds `127.0.0.1:80`.
- iptables `OUTPUT` chain `-d 127.0.0.1 -j RETURN` already bypasses the
  transparent proxy redirect for loopback traffic, so requests land on
  the server directly. No changes to the firewall rules.

`http://admin.airlock/` is a stable, discoverable entry point from
inside the sandbox; callers don't need to know about port numbers or IP
literals, and DNS is the only coordination surface.

### Module layout

```
app/airlockd/src/admin/
  mod.rs
  server.rs                         # axum Router + bind
  state.rs                          # AdminState { deny_tracker, tool_tracker }
  deny_tracker.rs                   # moved from net/deny_status.rs
  tool_tracker.rs                   # new: LRU of tool_use_id -> epoch
  routes.rs                         # re-exports
  routes/
    root.rs                         # GET /
    claude_hook_pre_tool_use.rs
    claude_hook_post_tool_use.rs
    claude_hook_post_tool_use_failure.rs
```

One handler per file — the route set is open-ended (more Claude hooks,
other tool integrations) and the one-route-one-file layout scales
without refactors.

### Routes

| Method + path                            | Behavior                                                                 |
|------------------------------------------|--------------------------------------------------------------------------|
| `GET /`                                  | `Airlock` (liveness + sanity check)                                      |
| `POST /claude/hooks/pre-tool-use`        | `ToolTracker.record(tool_use_id, now)`; respond `{}`                     |
| `POST /claude/hooks/post-tool-use`       | `ToolTracker.take(tool_use_id)`; respond `{}`                            |
| `POST /claude/hooks/post-tool-use-failure` | Correlate with `DenyTracker`; respond `hookSpecificOutput` or `{}`     |

`GET /last_deny` is gone. The `report_deny` RPC that feeds `DenyTracker`
is unchanged — only the exposure layer moved.

### Failure correlation

`claude_hook_post_tool_use_failure`:

1. Extract `tool_use_id` from the hook payload. Missing → pass through.
2. `tool_tracker.take(id)` → `None` (pre-hook never fired or cache
    evicted): pass through.
3. `deny_tracker.last()` → `None`: pass through.
4. `last_deny >= started_at` → return a `PostToolUseFailure`
    `hookSpecificOutput` with `additionalContext` asking Claude to
    surface the deny to the user and suggest editing `[[network.allow]]`
    rules. Otherwise pass through.

Comparison is `>=` so a deny reported in the same millisecond as the
tool start counts — the alternative would silently swallow a real deny
on a fast tool call. Both sides store Unix-epoch milliseconds; seconds
resolution would lose too many real overlaps for tools that fail in
well under a second, and the host/guest clocks share KVM's `kvm-clock`
so there's no meaningful drift at this granularity.

## ToolTracker

`quick_cache::sync::Cache<String, u64>` with capacity 1000. Claude
realistically has a handful of tool calls in flight; the cap exists as
a memory ceiling against a misbehaving client that never fires
post-hooks. `scc` was the alternative (already in the dep graph) but
it's unbounded — we specifically want eviction here. `quick_cache` is
already used by `network::tls` for per-host leaf certs, so no new
workspace dependency.

The tracker API is two methods: `record(id, epoch)` and `take(id) →
Option<u64>`. `take` doubles as the remove path — both successful
post-hooks and the failure correlator want to drop the entry, so a
single consuming API covers both callers.

## HTTP hook response format

Claude Code's HTTP hook protocol: 2xx + JSON body carries the decision;
non-2xx / connection errors are non-blocking. That matches the
fail-open posture we want — if airlockd is unreachable, tool calls
proceed normally.

For `PreToolUse` and `PostToolUse` (pass-through), we return `{}`. The
PreToolUse protocol allows a `hookSpecificOutput.permissionDecision:
"allow"`, but an empty object is equivalent and matches the "don't
modify tool use" guarantee we want.

For the failure correlator, when the deny condition is met:

```json
{
  "hookSpecificOutput": {
    "hookEventName": "PostToolUseFailure",
    "additionalContext": "A network request was denied by airlock policy ..."
  }
}
```

`additionalContext` is capped at 10,000 chars by the client; our
message is well inside that.

## Surface area removed

- `net/deny_status.rs` — deleted.
- `DENY_STATUS_PORT` constant in `airlock-common` — deleted.
- The docstring on `reportDeny` in `supervisor.capnp` that referenced
  the old port was rewritten to point at `http://admin.airlock/`.
- `docs/manual/src/configuration/network.md` — "Deny-status endpoint"
  section deleted; the new Claude hooks documentation lives in a
  dedicated `experimental-claude-hooks.md` page (linked from
  `SUMMARY.md` just above the Advanced Usage section) with a stability
  warning, the endpoint table, and a `.claude/settings.json` snippet.

## Not done

- No auth on the admin server. Loopback-only binding + container
  isolation is the boundary; if we ever expose endpoints that mutate
  host state we'll need to revisit.
- No rate limiting on hook endpoints. Claude fires one hook per tool
  call so volume is bounded by agent cadence.
- Command-style Claude hooks (not HTTP) are unsupported. The hook
  script could in principle shell out to `curl http://admin.airlock/…`
  but that's user glue — we only own the server side.
