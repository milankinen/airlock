# Experimental: Claude hooks

> This integration is experimental. Endpoint paths, payloads, and
> injected messages may change without notice.

When an airlock network policy denies a connection, the tool inside
the sandbox sees a generic failure — a DNS lookup that returns nothing,
a TCP connection refused, an HTTPS handshake that never completes.
From Claude's point of view that looks indistinguishable from a flaky
endpoint, a typo in a URL, or a transient outage. The usual response
is to retry, fall back to a different command, or invent a workaround
— anything except telling the user "your sandbox policy blocked this,
do you want to allow it?"

The supervisor already knows when it denied a connection. The Claude
Code [HTTP hooks protocol][ch-hooks] is the native way to feed that
knowledge back into the agent: hooks fire on tool lifecycle events
and can inject extra context into the model's view of the failure.
The endpoints below correlate denies with the tool calls that were
in flight when they happened, and surface a short explanation to
Claude so it can stop and ask instead of retrying blindly.

## Endpoints

The in-VM supervisor exposes an HTTP service at `http://admin.airlock/`
— the hostname resolves to loopback via the guest DNS server, and
loopback traffic bypasses the transparent proxy, so requests land on
the supervisor directly. Three of the admin endpoints implement the
hook protocol:

| Path                                  | Claude hook event    | Behavior                                                                                         |
|---------------------------------------|----------------------|--------------------------------------------------------------------------------------------------|
| `/claude/hooks/pre-tool-use`          | `PreToolUse`         | Record the tool's start time, keyed by `tool_use_id`                                             |
| `/claude/hooks/post-tool-use`         | `PostToolUse`        | Release the start-time record                                                                    |
| `/claude/hooks/post-tool-use-failure` | `PostToolUseFailure` | If any deny was reported since the tool started, inject `additionalContext` explaining the block |

The correlation is keyed on `tool_use_id`. The tracker holds up to
1000 in-flight tool calls; if Claude doesn't fire a post-hook for a
given id the entry is eventually evicted.

## Configuration

Add the three hooks to `.claude/settings.json` (either in your project
or globally in `~/.claude/settings.json`):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "http://admin.airlock/claude/hooks/pre-tool-use"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "http://admin.airlock/claude/hooks/post-tool-use"
          }
        ]
      }
    ],
    "PostToolUseFailure": [
      {
        "hooks": [
          {
            "type": "http",
            "url": "http://admin.airlock/claude/hooks/post-tool-use-failure"
          }
        ]
      }
    ]
  }
}
```

Non-2xx responses and connection errors from the admin endpoints are
non-blocking — if the supervisor is unreachable for any reason, tool
calls proceed as if the hooks weren't configured.

[ch-hooks]: https://code.claude.com/docs/en/hooks
