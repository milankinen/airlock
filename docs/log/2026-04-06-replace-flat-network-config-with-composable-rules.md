# Replace flat network config with composable rules

The flat network config (`allowed_hosts`, `tls_passthrough`, `host_ports`,
`middleware`) wasn't composable — presets couldn't cleanly layer their own
network rules. Replaced with a `network.rules` array where each rule is a
named group with allowed targets and optional per-target middleware.

### New config model

```toml
[[network.rules]]
name = "claude-code"
allow = ["api.anthropic.com", "claude.ai"]

[[network.rules]]
name = "localhost"
allow = ["localhost:8080"]

[[network.rules]]
name = "copilot-cli"
allow = ["github.com:443"]
[[network.rules.middleware."github.com"]]
script = "..."
```

### Key semantics

- **Target pattern**: `host[:port]`, wildcards supported, omitted port = all
- **Localhost detection**: rules with `localhost:*` targets replace `host_ports`,
  drive VM-side iptables
- **TLS passthrough**: implicit for targets without middleware; MITM for targets
  with middleware
- **Deny logic**: connection denied only if NO rule allows it (union of allows)

### Architecture: NetworkTarget

Introduced `NetworkTarget` as the resolved runtime model. At startup,
`rules::resolve()` flattens config rules into `Vec<NetworkTarget>` with
compiled Lua middleware attached. The proxy then queries targets directly
via `target::find_match()` — no more separate host_ports/tls_passthrough/
middleware fields on the Network struct.

Config (`config.rs`) is pure data — all network business logic lives in
`network/rules.rs` and `network/target.rs`.

### Naming: "middleware" everywhere

Renamed config `filters` → `middleware` for consistency with the existing
`http/middleware.rs` module and codebase conventions. The term "middleware"
is used consistently in config structs, target fields, rules resolution,
server, HTTP relay, test helpers, and documentation.
