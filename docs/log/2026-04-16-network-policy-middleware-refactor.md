# Network policy enum and middleware separation

## Policy replaces default_mode

Replaced the binary `DefaultMode` (`allow`/`deny`) with a four-value `Policy`
enum: `allow-always`, `deny-always`, `allow-by-default`, `deny-by-default`.

- `allow-always` (default): skip rules, allow everything
- `deny-always`: skip rules, deny everything including port forwards and sockets
- `allow-by-default`: allow unless explicitly denied by a rule
- `deny-by-default`: deny unless explicitly allowed by a rule

`deny-always` is the only policy that blocks port forwards and socket
forwarding — all other policies allow these infrastructure connections.

## Middleware separated from rules

Previously middleware was nested inside `NetworkRule` as
`[[network.rules.name.middleware]]`. This coupled middleware to specific
rules and made it impossible to share middleware across rules or apply
middleware independently.

Now middleware is a separate top-level section `[network.middleware.name]`
with its own `target` patterns, `env` variables, and `script`. Middleware
applies to any allowed connection whose host:port matches a middleware
target pattern, regardless of which rule allowed it.

This also enables middleware on localhost connections (previously impossible
since localhost short-circuited before middleware collection).

## Connection flow

1. `deny-always` → deny immediately (including sockets)
2. Localhost port-forward → remap port
3. `allow-always` → allow, collect matching middleware
4. Deny rules → deny wins unconditionally
5. Allow rules → allow, collect matching middleware
6. No match → `allow-by-default` allows, `deny-by-default` denies

## Changes

- `config.rs`: `Policy` enum, `MiddlewareRule` struct, removed `middleware`
  from `NetworkRule`, removed `NetworkMiddleware`
- `network/rules.rs`: `resolve()` no longer compiles middleware;
  new `resolve_middleware()` builds `MiddlewareTarget` list
- `network/target.rs`: `NetworkTarget` no longer carries middleware;
  new `MiddlewareTarget` struct
- `network.rs`: `resolve_target()` implements full policy flow with
  `collect_middleware()` helper
- `network/server.rs`: `deny-always` blocks socket connections
- CLI display updated for policy + separate middleware listing
- copilot-cli preset migrated to new middleware format
- All documentation updated (network.md, network-scripting.md, presets.md,
  DESIGN.md)
