# Network filtering via Rhai scripting

## Context

The VM has no network devices. All outbound TCP flows through the
host CLI via the `NetworkProxy` RPC capability (see `network-proxy`
plan). Before the CLI makes the real connection, we need a way for
users to filter, modify, or deny requests — and inject secrets from
environment variables.

**Rhai** is a pure-Rust, sandboxed-by-default scripting engine. It
can't do I/O, filesystem, or network unless we explicitly register
functions. It compiles to ~200KB of additional binary size and has
zero C dependencies.

The scripting layer lives entirely in the **CLI crate** (host side).
It intercepts at the `NetworkProxy::connect` boundary — after the
supervisor has extracted the destination host/port/tls from the
intercepted connection, but before the CLI opens a real connection.

```
Supervisor                         CLI (host)
    │                                  │
    │  connect(host, port, tls) ──────▶│
    │                                  ├─ Build Request object
    │                                  ├─ Run Rhai script
    │                                  │   ├─ return req  → allow (possibly modified)
    │                                  │   └─ return ()   → deny
    │                                  ├─ If allowed: open real TCP/TLS connection
    │◀── upstream ByteStream ──────────┤
    │                                  │
```

## Request object

The Rhai script receives a single `req` object with these fields:

```rhai
req.host       // "api.example.com"  — read/write
req.port       // 443                — read/write
req.tls        // true               — read-only
```

These fields map directly to the `NetworkProxy::connect` RPC params.
Modifying `host` or `port` lets a script reroute traffic (e.g.
staging → prod redirect, or localhost tunneling). `tls` is read-only
because changing it would break the already-established connection
between the container and the supervisor's MITM proxy.

### Future: HTTP-level fields

Once the supervisor decodes HTTP/1.1 (plain) or decrypted HTTPS
traffic, we can extend the request object with:

```rhai
req.method     // "GET"
req.path       // "/api/v1/users"
req.headers    // map: {"authorization": "Bearer ...", ...}
```

This is out of scope for the initial implementation but the types
should be designed to accommodate it (i.e. use an extensible struct,
not positional args).

## Built-in functions

Minimal set of registered Rhai functions:

| Function | Signature | Purpose |
|----------|-----------|---------|
| `env` | `env(key: &str) -> String` | Read host environment variable. Returns `""` if unset. |
| `log` | `log(msg: &str)` | Write to CLI's tracing output (debug level). |

`env` is the escape hatch for secrets injection — users store API
keys in env vars and the script injects them as headers or rewrites
hosts. The function reads from a filtered set of env vars (see
Security section).

## Script evaluation

### Lifecycle

1. **Load once** — on CLI startup, read the script file, compile it
   to an AST via `engine.compile()`. This catches syntax errors
   before the first connection.
2. **Eval per connection** — for each `NetworkProxy::connect` call,
   create a fresh `Scope`, bind `req`, and call `engine.eval_ast()`.
3. **Interpret result**:
   - Script returns the `req` object → **allow**, use (possibly
     modified) host/port for the real connection.
   - Script returns `()` → **deny**, close the RPC stream with an
     error.
   - Script panics or errors → **deny** and log the error. Fail
     closed.

### Performance

Rhai compiles to AST (no bytecode VM), so eval is fast but not
JIT-fast. For a network filter that runs once per TCP connection
this is irrelevant — even a complex script evaluates in <1ms, and
connection setup (TCP handshake, TLS) takes 10-100ms.

The engine itself is created once and reused. AST compilation
happens once at startup. Per-connection cost is just scope setup
and eval.

## Implementation plan

### Phase 1: Rhai engine core

New module: `cli/src/script/`

**`cli/src/script/mod.rs`** — public interface:

```rust
mod engine;
mod types;

pub use engine::ScriptEngine;
pub use types::ConnectRequest;
```

**`cli/src/script/types.rs`** — request type:

```rust
/// Represents a network connection request passed to the filter script.
/// Registered as a custom Rhai type so scripts can read/write fields.
#[derive(Debug, Clone)]
pub struct ConnectRequest {
    pub host: String,
    pub port: u16,
    pub tls: bool,
}
```

Register with Rhai via `CustomType` derive or manual registration:
- `get/set host`, `get/set port` — read/write
- `get tls` — read-only

**`cli/src/script/engine.rs`** — engine wrapper:

```rust
pub struct ScriptEngine {
    engine: rhai::Engine,
    ast: rhai::AST,
}

pub enum FilterResult {
    Allow(ConnectRequest),
    Deny,
}

impl ScriptEngine {
    /// Compile script from file. Fails fast on syntax errors.
    pub fn from_file(path: &Path, allowed_env: &[String]) -> Result<Self>;

    /// Evaluate the script for a connection request.
    pub fn eval(&self, req: ConnectRequest) -> Result<FilterResult>;
}
```

The constructor:
1. Creates a `rhai::Engine` with max operations limit (prevent
   infinite loops — `engine.set_max_operations(100_000)`).
2. Registers the `ConnectRequest` type and its accessors.
3. Registers `env()` — closes over a pre-captured `HashMap` of
   allowed env vars (not live `std::env` access, see Security).
4. Registers `log()`.
5. Compiles the script file to AST.

The `eval` method:
1. Creates a new `Scope` with `req` bound.
2. Calls `engine.eval_ast_with_scope()`.
3. Pattern-matches the `Dynamic` result:
   - `ConnectRequest` → `FilterResult::Allow(req)`
   - `()` → `FilterResult::Deny`
   - anything else → error

**`cli/Cargo.toml`:** add `rhai = "1"`.

**Deliverable:** unit tests that load a script string, eval with a
test `ConnectRequest`, and assert allow/deny/modification outcomes.

### Phase 2: Configuration

The script file path and env var allowlist come from config. For now,
hardcode the convention and extend `Config` minimally:

**`cli/src/config.rs`:**

```rust
pub struct Config {
    // ... existing fields ...
    pub network_filter: Option<PathBuf>,
    pub filter_env: Vec<String>,  // env var names to expose to scripts
}
```

- `network_filter`: path to `.rhai` script file. If `None`, all
  connections are allowed (no scripting overhead).
- `filter_env`: explicit list of env var names the script can read.
  Empty = no env access.

These will eventually come from `ez.toml` (future config file work).
For now they can be wired from CLI args or a simple project-level
file.

### Phase 3: Integration with NetworkProxy

This phase depends on the networking plumbing being ready. It wires
the `ScriptEngine` into the `NetworkProxy` implementation.

**`cli/src/rpc/network.rs`** (the `NetworkProxy::Server` impl):

```rust
struct NetworkProxyImpl {
    script: Option<Rc<ScriptEngine>>,
    // ... connection-making infrastructure ...
}

impl network_proxy::Server for NetworkProxyImpl {
    async fn connect(self: Rc<Self>, params, mut results) {
        let host = params.get()?.get_host()?.to_string()?;
        let port = params.get()?.get_port();
        let tls = params.get()?.get_tls();

        let req = ConnectRequest { host, port, tls };

        // Apply filter script (if configured)
        let req = match &self.script {
            None => req,  // no script = allow all
            Some(engine) => match engine.eval(req)? {
                FilterResult::Allow(req) => req,
                FilterResult::Deny => {
                    // Return error / close stream
                    return Err(capnp::Error::failed("connection denied by filter"));
                }
            },
        };

        // Proceed with (possibly modified) req.host / req.port
        // ... open real TCP/TLS connection ...
    }
}
```

**`cli/src/main.rs`:** load the script engine during startup:

```rust
let script = match &project.config.network_filter {
    Some(path) => Some(ScriptEngine::from_file(path, &project.config.filter_env)?),
    None => None,
};
// pass to NetworkProxyImpl
```

### Phase 4: Error reporting

Script errors should be visible to the user but not crash the CLI.

- **Compilation errors** (phase 1 load): fatal, exit with a clear
  message pointing to the script file and line/column.
- **Runtime errors** (per-connection eval): log via `tracing::warn!`,
  deny the connection (fail closed), container sees a connection
  refused.
- **Timeout** (infinite loop): Rhai's `max_operations` limit
  triggers a runtime error, handled same as above.

## Script examples

### Allow only specific hosts

```rhai
// allow.rhai
let allowed = ["api.github.com", "registry.npmjs.org", "dl-cdn.alpinelinux.org"];

if !(req.host in allowed) {
    log(`denied: ${req.host}:${req.port}`);
    return ();
}

req
```

### Inject auth token via env var

```rhai
// inject_token.rhai — for future HTTP-level filtering
let token = env("API_TOKEN");

if req.host == "api.internal.com" && token != "" {
    req.headers["authorization"] = `Bearer ${token}`;
}

req
```

### Reroute to staging

```rhai
// staging.rhai
if req.host == "api.prod.com" {
    req.host = "api.staging.com";
    log("rerouted to staging");
}

req
```

## Security

### Sandbox guarantees

Rhai provides these by default (no opt-out needed):
- No filesystem access
- No network access
- No process spawning
- No FFI / unsafe
- No module imports (can be enabled, but we won't)

We additionally configure:
- **`set_max_operations(100_000)`** — prevents infinite loops
- **`set_max_string_size(1_048_576)`** — 1MB, prevents memory bombs
- **`set_max_array_size(10_000)`** — prevents memory bombs

### Environment variable scoping

The `env()` function does **not** call `std::env::var` at runtime.
Instead, at engine creation time, we snapshot the values of only the
env vars listed in `filter_env` config into a `HashMap<String, String>`.
The `env()` function reads from this map. This means:

- Scripts can only see explicitly allowed env vars
- No ambient access to PATH, HOME, AWS credentials, etc.
- The snapshot is immutable — scripts can't mutate env state

### Fail-closed

Any error during script evaluation results in connection denial.
A misconfigured or buggy script blocks traffic rather than allowing
it through unfiltered.

## Testing

Unit tests (no VM needed):

1. **Allow passthrough** — empty script or `req` return allows
2. **Deny** — `()` return denies
3. **Modify host** — `req.host = "other"; req` changes target
4. **Modify port** — `req.port = 8080; req` changes target
5. **env() reads allowed vars** — pre-set var, verify script sees it
6. **env() hides unallowed vars** — var exists but not in allowlist
7. **Infinite loop** — hits max_operations, returns error
8. **Syntax error** — compile fails with clear message
9. **Runtime error** — bad script returns Deny (fail-closed)
10. **tls is read-only** — assignment to `req.tls` errors

## Dependencies

**CLI only:**
- `rhai = "1"` (latest 1.x)

No changes to protocol or supervisor crates.
