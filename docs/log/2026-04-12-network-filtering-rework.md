# Network filtering rework: simplified allow/deny rules

## Summary

Simplified network rule configuration and connection filtering logic.
Removed `mode`, `targets`, `RuleMode`, `DefaultMode`, `BaseAction`, and
`http:` target prefixes in favour of a plain `allow`/`deny` list model.

## Motivation

The previous design had per-rule `mode = "allow"|"deny"` with a single
`targets` list, plus a global `default_mode`. This required understanding
three interacting concepts to reason about a connection. The new model
reduces it to two lists and one rule: **deny wins, allow required**.

## Rule format (new)

```toml
[network.rules.my-rule]
allow = ["api.example.com:443", "*.cdn.example.com"]
deny  = ["bad.example.com"]          # optional, wins unconditionally

[[network.rules.my-rule.middleware]] # optional, triggers TLS intercept
script = "if req.path == '/admin' then req:deny() end"
```

- `allow`: hosts/ports to permit. Required for a connection to go through.
- `deny`: hosts/ports to block unconditionally (checked first, no middleware).
- `middleware`: HTTP scripts attached to allow targets. Default: forward.
  Call `req:deny()` to block.

The `http:` prefix on targets was removed — it only gated non-HTTP traffic,
which added complexity without much value. Protocol is now irrelevant to rule
matching.

## Connection decision logic

```
TCP connection (host, port)
│
├─ any deny pattern matches? → DENY immediately (deny wins)
├─ no allow pattern matches? → DENY
└─ allow matches            → collect middleware from all matching allow rules
│
├─ middleware empty → passthrough (raw TCP relay, no TLS MITM)
│
├─ TLS? → MITM intercept; else plain TCP
│
├─ HTTP detected → run middleware chain (allow-by-default):
│   ├─ req:deny()  → 403
│   ├─ req:send()  → forward
│   └─ fallthrough → forward implicitly
│
└─ non-HTTP → raw TCP relay
```

## Code changes

### `config.rs`
- Removed `RuleMode` and `DefaultMode` enums.
- `NetworkRule`: replaced `mode: RuleMode` + `targets: Vec<String>` with
  `allow: Vec<String>` and `deny: Vec<String>`.
- `Network`: removed `default_mode` field.

### `target.rs`
- Removed `BaseAction` enum.
- `NetworkTarget`: removed `base_action` and `http_only` fields.
- `ResolvedTarget`: replaced `tcp_action: BaseAction` with `allowed: bool`.
- `is_passthrough()`: `self.allowed && self.middleware.is_empty()`.

### `rules.rs`
- `resolve()` returns `(allow_targets, deny_targets)` instead of a flat list.
- `parse_target()` simplified: no `http:` prefix stripping.
- `localhost_ports_from_config` iterates `rule.allow` only.

### `network.rs`
- `Network` holds `allow_targets` and `deny_targets` separately.
- `resolve_target()`: deny patterns checked first (deny wins); then allow
  patterns collected with middleware; `allowed = any_allow`.

### `http/middleware.rs`
- Removed `fallback_action`/`base_action` parameter from `run()`.
- Removed `req:allow()` method (was only meaningful for deny-mode rules).
- Middleware is always allow-by-default: fallthrough = implicit forward.

### `server.rs`
- Deny check: `if !net_target.allowed { ... return Denied }`.
- Removed `http_only` and `tcp_action == Deny` checks from connection handler.

### Presets (all 11 files)
- `mode = "allow"\ntargets = [...]` → `allow = [...]`.
- Stripped `http:` prefix from all target strings.

### `airlock.toml`
- `allow-all` rule updated to `allow = ["*"]`.
