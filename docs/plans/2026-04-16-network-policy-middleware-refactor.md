# Plan: Refactor network config — policy enum, separate middleware from rules

## Context

The current network config conflates middleware with rules (`NetworkRule` has
both allow/deny patterns AND middleware scripts). This creates awkward coupling:
middleware only applies to targets in the same rule, and you can't share
middleware across multiple rules. The `default_mode` field only has two values
(`allow`/`deny`) and doesn't clearly express all desired policies.

The refactoring separates these concerns and introduces a richer policy enum.

## Target config format

```toml
[network]
# allow-always:    skip rules, allow everything
# deny-always:     skip rules, deny everything (including port forwards + sockets)
# allow-by-default: allow unless explicitly denied by rules
# deny-by-default:  deny unless explicitly allowed by rules
policy = "allow-always"

[network.rules.example]
enabled = true
allow = ["example.org:443"]
deny = []

[network.middleware."my-middleware"]
enabled = true
target = ["example.org:443"]
env.TOKEN = "${MY_API_KEY}"
script = """
if not env.TOKEN then req:deny() end
req:setHeader("Authorization", "Bearer " .. env.TOKEN)
"""
```

## Design

### New flow: on connect

1. **Policy check** — `deny-always` denies everything immediately (including
   port forwards and sockets). `allow-always` skips rules entirely.
2. **Localhost/port-forward check** — if localhost IP and port is forwarded,
   remap port. If policy is `deny-always`, this is already denied above.
3. **Rule evaluation** — deny rules checked first (deny wins). Then allow
   rules. If no rule matches, `allow-by-default` allows, `deny-by-default` denies.
4. **Middleware application** — if the connection was allowed and target matches
   any middleware `target` patterns, collect those middleware scripts. This now
   applies to localhost too (since middleware is decoupled from rules).

### Key change: `resolve_target` returns without middleware

`resolve_target()` determines allowed/denied and port remapping only.
A new method `resolve_middleware()` collects middleware from matching
`network.middleware` entries. The server calls both in sequence.

## Detailed changes

### 1. Config: replace `DefaultMode` with `Policy`, separate middleware

**File:** `crates/airlock/src/config.rs`

Replace `DefaultMode`:
```rust
#[derive(Debug, Clone, Copy, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Policy {
    /// Skip rules, allow all connections.
    #[default]
    AllowAlways,
    /// Skip rules, deny all connections (including port forwards and sockets).
    DenyAlways,
    /// Allow connections unless explicitly denied by a rule.
    AllowByDefault,
    /// Deny connections unless explicitly allowed by a rule.
    DenyByDefault,
}
```

Update `Network` struct:
```rust
pub struct Network {
    pub policy: Policy,            // was default_mode: DefaultMode
    pub rules: BTreeMap<String, NetworkRule>,
    pub middleware: BTreeMap<String, MiddlewareRule>,  // NEW
    pub ports: BTreeMap<String, PortForward>,
    pub sockets: BTreeMap<String, SocketForward>,
}
```

Remove `middleware` field from `NetworkRule`:
```rust
pub struct NetworkRule {
    pub enabled: bool,
    pub allow: Vec<String>,
    pub deny: Vec<String>,
    // middleware: Vec<NetworkMiddleware> — REMOVED
}
```

New `MiddlewareRule` struct (replaces `NetworkMiddleware` embedded in rules):
```rust
pub struct MiddlewareRule {
    pub enabled: bool,
    pub target: Vec<String>,  // host[:port] patterns
    pub env: BTreeMap<String, String>,
    pub script: String,
}
```

Remove the old `NetworkMiddleware` struct entirely.

### 2. Update `rules::resolve()` — split into rules + middleware

**File:** `crates/airlock/src/network/rules.rs`

`resolve()` no longer compiles middleware. It only builds allow/deny target
lists (without middleware field on `NetworkTarget`).

New function `resolve_middleware()` compiles middleware from the new
`network.middleware` config section, returning middleware targets:
```rust
pub struct MiddlewareTarget {
    pub host: String,
    pub port: Option<u16>,
    pub middleware: CompiledMiddleware,  // single compiled script
}

pub fn resolve_middleware(
    network: &Network,
    log: &LogFn,
) -> anyhow::Result<Vec<MiddlewareTarget>> {
    // For each enabled middleware rule, compile the script,
    // then create a MiddlewareTarget for each target pattern
}
```

### 3. Update `NetworkTarget` — remove middleware

**File:** `crates/airlock/src/network/target.rs`

`NetworkTarget` no longer carries middleware:
```rust
pub struct NetworkTarget {
    pub host: String,
    pub port: Option<u16>,
    // middleware removed
}
```

`ResolvedTarget` keeps middleware (collected from `MiddlewareTarget`s):
```rust
pub struct ResolvedTarget {
    pub host: String,
    pub port: u16,
    pub middleware: Vec<CompiledMiddleware>,
    pub allowed: bool,
}
```

### 4. Update `Network` struct and resolution

**File:** `crates/airlock/src/network.rs`

```rust
pub struct Network {
    policy: Policy,
    tls_client: Arc<rustls::ClientConfig>,
    interceptor: Rc<tls::TlsInterceptor>,
    allow_targets: Vec<NetworkTarget>,
    deny_targets: Vec<NetworkTarget>,
    middleware_targets: Vec<MiddlewareTarget>,  // NEW
    port_forwards: HashMap<u16, u16>,
    pub(super) socket_map: HashMap<String, PathBuf>,
}
```

Updated `resolve_target()`:
```rust
pub fn resolve_target(&self, host: &str, port: u16) -> ResolvedTarget {
    // 1. deny-always → deny immediately
    if matches!(self.policy, Policy::DenyAlways) {
        return denied(host, port);
    }

    // 2. Localhost port-forward remapping
    let (host, port) = if is_localhost(host) {
        if let Some(&host_port) = self.port_forwards.get(&port) {
            ("127.0.0.1", host_port)
        } else {
            (host, port)
        }
    };

    // 3. allow-always → allow, collect middleware
    if matches!(self.policy, Policy::AllowAlways) {
        return ResolvedTarget {
            host, port,
            middleware: self.collect_middleware(host, port),
            allowed: true,
        };
    }

    // 4. Deny rules (win unconditionally)
    for target in &self.deny_targets {
        if target.matches(host, port) {
            return denied(host, port);
        }
    }

    // 5. Allow rules
    let any_allow = self.allow_targets.iter().any(|t| t.matches(host, port));
    let allowed = any_allow || matches!(self.policy, Policy::AllowByDefault);

    let middleware = if allowed {
        self.collect_middleware(host, port)
    } else {
        vec![]
    };

    ResolvedTarget { host, port, middleware, allowed }
}

fn collect_middleware(&self, host: &str, port: u16) -> Vec<CompiledMiddleware> {
    self.middleware_targets
        .iter()
        .filter(|mt| mt.matches(host, port))
        .map(|mt| mt.middleware.clone())
        .collect()
}
```

### 5. Update server: deny-always blocks sockets too

**File:** `crates/airlock/src/network/server.rs`

In the `connect()` RPC handler, check policy before socket connections:
```rust
connect_target::Socket(guest_path) => {
    if matches!(self.policy(), Policy::DenyAlways) {
        // deny socket connections too
        results.get().init_result().set_denied("denied by policy");
        return Ok(());
    }
    // ... existing socket logic
}
```

Expose policy via a method or make it accessible.

### 6. Update CLI display

**Files:** `crates/airlock/src/cli/cmd_start.rs`, `crates/airlock/src/cli/cmd_show.rs`

Replace `default_mode` display with `policy`. Update middleware display
to show the new separate middleware section instead of per-rule middleware.

### 7. Update test helpers

**File:** `crates/airlock/src/network/tests/helpers.rs`

- `TestNetworkConfig` needs updating: middleware is no longer attached to rules
- `build_network()` needs to compile middleware separately and pass as
  `middleware_targets` to `Network`
- Config struct initialization uses `Policy` instead of `DefaultMode`

### 8. Update bats tests

**Files:** `tests/vm/middleware.bats`, `tests/vm/network.bats`

Replace `default_mode = "deny"` with `policy = "deny-by-default"`.
Move middleware from `[[network.rules.*.middleware]]` to `[network.middleware.*]`.

### 9. Update documentation

**Files:**
- `docs/manual/src/configuration/network.md` — update policy, rules, middleware sections
- `docs/manual/src/configuration/presets.md` — update preset example

## Files to modify

1. `crates/airlock/src/config.rs` — `Policy` enum, `MiddlewareRule`, update `Network`/`NetworkRule`
2. `crates/airlock/src/network/rules.rs` — split resolve, add `resolve_middleware()`
3. `crates/airlock/src/network/target.rs` — remove middleware from `NetworkTarget`, add `MiddlewareTarget`
4. `crates/airlock/src/network.rs` — new resolution logic with policy + middleware collection
5. `crates/airlock/src/network/server.rs` — deny-always blocks sockets
6. `crates/airlock/src/cli/cmd_start.rs` — display updates
7. `crates/airlock/src/cli/cmd_show.rs` — display updates
8. `crates/airlock/src/network/tests/helpers.rs` — test harness updates
9. `tests/vm/middleware.bats` — new config format
10. `tests/vm/network.bats` — new config format
11. `docs/manual/src/configuration/network.md` — documentation
12. `docs/manual/src/configuration/presets.md` — preset example

## Verification

1. `mise run lint` — no lint/format issues
2. `mise run test` — unit tests pass
3. VM bats tests pass with updated config format
