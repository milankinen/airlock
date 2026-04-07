# Simplify network filtering to host allowlist

Replaced `default_mode` + `tcp_connect` Lua scripts with a simple
`allowed_hosts` pattern list. Empty list = deny all traffic. Supports
`*` (allow all), exact match, and `*.domain.com` wildcards.

Removed: `NetworkMode` enum, `NetworkRuleType` enum, `tcp_connect`
script type, `ConnectRequest` Lua userdata. The TCP connect step now
just checks `is_host_allowed()` — no Lua involved.

HTTP request scripts simplified: requests are allowed by default,
scripts can only deny (no explicit `req:allow()` needed).

Renamed `allowed_hosts_tls` to `tls_passthrough` for clarity.
