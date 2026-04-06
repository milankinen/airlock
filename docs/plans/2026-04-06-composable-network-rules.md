# Rework network config to composable rules

## Context

Current flat network config (`allowed_hosts`, `tls_passthrough`, `host_ports`, `middleware`) isn't composable — presets can't cleanly add their own rules. Replace with `network.rules` array where each rule is a named group with allow targets and optional per-target filters.

## New config model

```toml
[[network.rules]]
name = "claude-code"
allow = ["api.anthropic.com", "claude.ai"]

[[network.rules]]
name = "localhost"
allow = ["localhost:8080", "localhost:9999"]

[[network.rules]]
name = "copilot-cli"
allow = ["github.com:443", "api.github.com:443"]
[[network.rules.filters."github.com"]]
script = "if req:path ~= '/login' then req:deny() end"
```

## Key semantics

- **Target pattern**: `host[:port]`, wildcards supported, omitted port = `*`
- **Localhost**: rules with `localhost:*` targets replace `host_ports`, drive VM iptables
- **TLS passthrough**: implicit for targets WITHOUT filters; MITM for targets WITH filters
- **Deny logic**: connection denied only if NO rule allows it (union of all allows)

## Changes

### 1. Config structs (`cli/src/config.rs`)

Replace Network fields:
```rust
struct Network {
    rules: Vec<NetworkRule>,
}
struct NetworkRule {
    name: String,
    allow: Vec<String>,  // "host[:port]" patterns
    filters: HashMap<String, Vec<NetworkFilter>>,
}
struct NetworkFilter {
    script: String,
}
```

### 2. Presets (`cli/src/config/presets/*.toml`)

Update claude-code, copilot-cli, codex to use `[[network.rules]]` format.

### 3. Network proxy (`cli/src/network/`)

- `network.rs` setup: build allowed hosts, localhost ports, TLS passthrough, and middleware from rules
- `middleware.rs`: derive `allowed_hosts` and compiled HTTP rules from rules
- `server.rs`: same validation logic but sourced from flattened rules
- TLS passthrough: target has filters → MITM, no filters → passthrough

### 4. RPC/Supervisor (`cli/src/rpc/supervisor.rs`, `sandbox/supervisor/`)

- `host_ports` derived from rules targeting `localhost:*` 
- `tls_passthrough` sent to supervisor unchanged (still needed for logging)
- Cap'n Proto schema may need minor update

### 5. Tests

- Update existing network tests that construct config
- Update config test fixtures

## Verification

- `mise run lint` passes
- `mise run build:supervisor` passes  
- All existing tests pass with updated config format
- Test with `ez.local.toml` using new rule format
