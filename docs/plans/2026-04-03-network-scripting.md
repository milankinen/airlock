# Network filtering via Lua scripting

## Context

All outbound traffic from the VM flows through the CLI's NetworkProxy.
Before making real connections, we need scriptable filtering: allow/deny
connections, inject auth headers, reroute traffic. Scripts are inline
Lua (LuaJIT via mlua) defined as rules in ez.local.toml.

## Permission model

For each connection, all matching rules execute in order:
1. If any rule calls `req:deny()` → **denied immediately** (short-circuit)
2. After all rules run, if any called `req:allow()` → **allowed**
3. Otherwise, use `network.default_mode` config (`"allow"` or `"deny"`,
   default `"deny"`)

This means deny always wins, allow must be explicit (unless default is
allow), and scripts can modify the request along the way.

## Rule configuration (ez.local.toml)

```toml
[network]
default_mode = "deny"  # or "allow"

[[network.rules]]
name = "GitHub auth"
type = "tcp_connect"          # tcp_connect | http_request | http_response
env.GITHUB_TOKEN = "GitHub personal access token"
script = """
if req:hostMatches("*.github.com") then
  req:allow()
end
"""

[[network.rules]]
name = "Inject GitHub token"
type = "http_request"
env.GITHUB_TOKEN = "GitHub personal access token"
script = """
if req:hostMatches("*.github.com") then
  req.headers["Authorization"] = "Bearer " .. env.GITHUB_TOKEN
  req:allow()
end
"""
```

### Rule types

- **`tcp_connect`** — runs before TCP connection. Has: host, port, tls.
  Can allow/deny and modify host/port.
- **`http_request`** — runs after HTTP request is parsed from the relay
  stream. Has: host, port, tls, method, path, headers. Can modify
  headers, path. CLI intercepts HTTP bytes from the relay.
- **`http_response`** — future, out of scope for initial impl.

### Env var declarations

Each rule declares required env vars with descriptions. At CLI startup,
all required env vars are validated — **hard error** if any are missing.
The env values are snapshotted into a table accessible as `env.VAR_NAME`
in the script. Scripts cannot access env vars not declared in their rule.

## Lua API exposed to scripts

### Request object (`req`)

```lua
-- tcp_connect type:
req.host         -- string, read/write
req.port         -- number, read/write
req.tls          -- boolean, read-only

-- http_request type (extends tcp_connect):
req.method       -- string, read-only
req.path         -- string, read/write
req.headers      -- table, read/write (case-insensitive keys)

-- Methods:
req:allow()      -- mark request as allowed
req:deny()       -- mark request as denied (short-circuits all rules)
req:hostMatches(pattern) -- glob match, e.g. "*.github.com"
```

### Global functions

```lua
env.VAR_NAME     -- access declared env vars (table, read-only)
log(msg)         -- write to CLI tracing output (debug level)
```

### Sandbox

- Remove `os`, `io`, `debug`, `loadfile`, `dofile`, `load` from globals
- Set instruction count hook via `HookTriggers` (limit ~1M instructions)
- No filesystem, network, or process access
- Each rule gets its own Lua state (no cross-rule contamination)

## Implementation plan

### Phase 1: Config + rule loading

**Modify: `cli/src/config.rs`**

Add to Network config:
```rust
#[config(default_t = "deny".into())]
pub default_mode: String,  // "allow" or "deny"
#[config(default)]
pub rules: Vec<NetworkRule>,
```

NetworkRule (serde-based for Vec<T>):
```rust
pub struct NetworkRule {
    pub name: String,
    pub r#type: String,  // "tcp_connect" | "http_request"
    pub env: HashMap<String, String>,  // var_name → description
    pub script: String,
}
```

### Phase 2: Lua engine + sandbox

**New: `cli/src/network/scripting.rs`**

```rust
pub struct ScriptEngine {
    rules: Vec<CompiledRule>,
    default_mode: DefaultMode, // Allow | Deny
}

struct CompiledRule {
    name: String,
    rule_type: RuleType,
    lua: mlua::Lua,  // per-rule Lua state
    // env vars are set as globals in the Lua state
}

pub enum FilterResult {
    Allow(ConnectRequest),
    Deny(String),        // reason
    Default,             // no rule decided
}
```

Engine creation:
1. For each rule, create a sandboxed `Lua` instance
2. Strip dangerous globals (os, io, debug, load, etc.)
3. Snapshot declared env vars into `env` table
4. Validate all required env vars exist (hard error if missing)
5. Pre-load the script (syntax check at startup)

Evaluation (per connection):
1. Build request userdata from connection params
2. Run each rule's script with the request
3. If any calls `req:deny()` → return Deny immediately
4. After all rules: if any called `req:allow()` → return Allow
5. Otherwise → return Default (caller checks default_mode)

### Phase 3: Integration with NetworkProxy

**Modify: `cli/src/network/mod.rs`** — setup creates ScriptEngine

**Modify: `cli/src/network/server.rs`** — before connecting:

```rust
// In connect():
let req = ConnectRequest { host, port, tls };
match self.script_engine.eval_tcp_connect(req)? {
    FilterResult::Allow(req) => { /* proceed with req */ }
    FilterResult::Deny(reason) => {
        return Err(capnp::Error::failed(format!("denied: {reason}")));
    }
    FilterResult::Default => {
        match self.default_mode {
            DefaultMode::Allow => { /* proceed */ }
            DefaultMode::Deny => {
                return Err(capnp::Error::failed("denied by default policy"));
            }
        }
    }
}
```

### Phase 4: HTTP-level interception (follow-up)

For `http_request` rules, the CLI needs to intercept the HTTP bytes
from the relay stream before forwarding. This requires:
1. Buffer the first bytes from the container
2. Parse HTTP request line + headers (using `httparse`)
3. Run http_request rules (can modify headers)
4. Reconstruct and forward the modified request

This is more complex and can be a separate PR after tcp_connect works.

## Files to modify/create

- `cli/Cargo.toml` — add `mlua` with luajit+vendored features
- `cli/src/config.rs` — add NetworkRule, default_mode to Network
- `cli/src/network/scripting.rs` — new module: Lua engine + sandbox
- `cli/src/network/request.rs` — request userdata type
- `cli/src/network/mod.rs` — create ScriptEngine during setup
- `cli/src/network/server.rs` — call engine before connecting

## Script examples

### Allow specific hosts only
```lua
local allowed = {"github.com", "npmjs.org", "*.alpine.org"}
for _, pattern in ipairs(allowed) do
  if req:hostMatches(pattern) then
    req:allow()
    return
  end
end
```

### Reroute to staging
```lua
if req.host == "api.prod.com" then
  req.host = "api.staging.com"
  req:allow()
  log("rerouted to staging")
end
```

### Inject auth header (http_request type)
```lua
if req:hostMatches("*.github.com") then
  req.headers["Authorization"] = "Bearer " .. env.GITHUB_TOKEN
  req:allow()
end
```

## Verification

```bash
# Test deny-by-default (no rules)
echo '[network]' > ez.local.toml
mise run ez -- wget http://example.com  # should fail

# Test allow rule
cat > ez.local.toml << 'EOF'
[[network.rules]]
name = "allow example"
type = "tcp_connect"
script = """
if req:hostMatches("example.com") then req:allow() end
"""
EOF
mise run ez -- wget -qO- http://example.com  # should work

# Test deny rule
cat > ez.local.toml << 'EOF'
[network]
default_mode = "allow"
[[network.rules]]
name = "block example"
type = "tcp_connect"
script = """
if req:hostMatches("example.com") then req:deny() end
"""
EOF
mise run ez -- wget http://example.com  # should fail

# Test missing env var
cat > ez.local.toml << 'EOF'
[[network.rules]]
name = "needs token"
type = "tcp_connect"
env.MISSING_VAR = "This var doesn't exist"
script = "req:allow()"
EOF
mise run ez  # should hard error at startup
```

## Dependencies

- `mlua = { version = "0.11", features = ["luajit", "vendored"] }`

---

# Hierarchical TOML configuration

## Context

The CLI currently uses a hardcoded `Config::default()` overridden by
CLI flags. We need proper config file support with hierarchical
loading, so users can set defaults globally or per-project.

## Config file loading order (later merges over former)

1. `~/.ezpez/config.toml` — global defaults
2. `~/.ez.toml` — user-level shorthand
3. `<project-root>/ez.toml` — project config (committed)
4. `<project-root>/ez.local.toml` — local overrides (gitignored)

## Merge rules

- **Arrays**: concatenate (former ++ latter)
- **Objects**: recursive merge
- **Primitives**: override (latter wins)
- **Type mismatch**: override with latter value

## Architecture

```
TOML files → serde_json::Value (via toml crate)
           → custom merge (arrays concat, objects recursive)
           → single merged serde_json::Value
           → Json source for smart-config
           → smart-config validates + deserializes → Config struct
```

CLI flags are applied AFTER config file loading (override config).

## Config struct (smart-config)

```rust
#[derive(Debug, DescribeConfig, DeserializeConfig)]
struct Config {
    #[config(default_t = "alpine:latest".into())]
    image: String,
    #[config(default_t = 2)]
    cpus: u32,
    #[config(default_t = 512)]
    memory_mb: u64,
    #[config(default)]
    verbose: bool,
    #[config(nest)]
    network: NetworkConfig,
    #[config(default)]
    mounts: Vec<MountConfig>,
}

#[derive(Debug, DescribeConfig, DeserializeConfig)]
struct NetworkConfig {
    #[config(default)]
    host_ports: Vec<u16>,
}

#[derive(Debug, DescribeConfig, DeserializeConfig)]
struct MountConfig {
    source: String,
    target: String,
    #[config(default)]
    read_only: bool,
}
```

Runtime-only fields (`args`, `terminal`) are NOT in the config file
schema — they're set by the CLI after loading.

## Implementation plan

### Phase 1: Config loading + merge (`cli/src/config.rs`)

Rewrite `config.rs` to:

1. Define `Config`, `NetworkConfig`, `MountConfig` with smart-config
   derive macros (`DescribeConfig`, `DeserializeConfig`)
2. Add `load(project_root: &Path) -> anyhow::Result<Config>` function:
   - Build list of config paths (in order)
   - For each existing file: read → `toml::from_str` → `serde_json::Value`
   - Merge all values with custom `merge_json()` function
   - Pass merged value to smart-config via `Json` source
   - Parse and return typed `Config`
3. Add `merge_json(base: Value, overlay: Value) -> Value` function:
   - Both objects: recursive merge keys
   - Both arrays: concatenate
   - Otherwise: overlay wins
4. Add runtime fields separately (not in smart-config schema):
   - `args: Vec<String>` — from CLI trailing args
   - `terminal: bool` — from TTY detection

The `Config` struct keeps `args` and `terminal` as regular fields
set after `load()` returns. Or use a wrapper:

```rust
pub struct RuntimeConfig {
    pub config: Config,  // from files + smart-config
    pub args: Vec<String>,
    pub terminal: bool,
}
```

### Phase 2: Update main.rs

```rust
let cli = cli::Cli::parse();
let cwd = std::env::current_dir()?;
let mut config = config::load(&cwd)?;
// CLI flags override config file values
if cli.cpus != 2 { config.cpus = cli.cpus; }
if cli.memory != 512 { config.memory_mb = cli.memory; }
if cli.verbose { config.verbose = true; }
```

Or better: merge CLI flags as another JSON layer before smart-config
parsing, so smart-config handles all the defaults.

### Phase 3: Update consumers

All code accessing `project.config.*` stays the same — the `Config`
struct has the same field names. Just need to handle `args` and
`terminal` being separate.

## Files to modify

- `cli/src/config.rs` — rewrite with smart-config + TOML loading
- `cli/src/main.rs` — use `config::load()` instead of manual construction
- `cli/src/project.rs` — adjust for new Config (args/terminal handling)
- `cli/Cargo.toml` — already has `smart-config` and `toml`

## Example config file (`ez.toml`)

```toml
image = "alpine:latest"
cpus = 4
memory_mb = 1024

[network]
host_ports = [8080, 3000]

[[mounts]]
source = "./data"
target = "/data"
read_only = true
```

## Verification

```bash
# Create a test config
echo 'cpus = 4' > ez.toml
mise run ez -- nproc  # should show 4

# Test merge
echo 'cpus = 8' > ez.local.toml
mise run ez -- nproc  # should show 8

# Test array concat
echo -e '[network]\nhost_ports = [8080]' > ez.toml
echo -e '[network]\nhost_ports = [3000]' > ez.local.toml
# host_ports should be [8080, 3000]

# Test global config
echo 'verbose = true' > ~/.ez.toml
mise run ez  # should show debug output
```

---

# Virtual DNS server for the VM

## Context

The VM has no network devices. DNS doesn't work inside the container
because there's no real network stack. The transparent TCP proxy
handles connections but relies on TLS SNI for hostname discovery —
plain HTTP connections only see the destination IP.

We implement a virtual DNS server that assigns synthetic IPs to
hostnames. The proxy reverse-lookups these IPs to recover the
hostname, giving it full visibility for all connections.

## Architecture

```
Container: curl http://example.com
  │
  ├─ DNS query (UDP) ──→ Supervisor DNS (10.0.0.1:53)
  │                        ├─ Allocate 10.2.0.1 for "example.com"
  │                        └─ Return A record: 10.2.0.1
  │
  ├─ TCP connect to 10.2.0.1:80
  │   iptables REDIRECT → supervisor:15001
  │
  ▼
Supervisor proxy (15001)
  ├─ SO_ORIGINAL_DST → 10.2.0.1
  ├─ Reverse lookup → "example.com"
  ├─ RPC: network.connect("example.com", 80, ...)
  ▼
CLI (host) → real DNS + real connection
```

## Plan

### Phase 1: DNS resolver state + server

**New file: `sandbox/supervisor/src/net/dns.rs`**

`DnsState` — bidirectional hostname ↔ virtual IP mapping:
- Uses `scc::HashMap` for lock-free concurrent lookups (no
  `Rc<RefCell>` needed — cleaner API)
- `allocate(hostname) -> Ipv4Addr` — returns cached IP or assigns
  next from 10.2.0.0/16 block (starting at 10.2.0.1)
- `reverse(ip) -> Option<String>` — proxy uses this for lookups
- Special case: "localhost" → 127.0.0.1 (no allocation)
- Shared via `Rc<DnsState>` (counter for next IP uses `Cell<u32>`)

`start(state)` — spawns UDP listener on 10.0.0.1:53:
- Parse DNS query with `simple-dns` crate (`Packet::parse`)
- A record query: allocate virtual IP, build response with
  `Packet::new_reply` + `ResourceRecord` A answer
- AAAA query: empty response (forces IPv4 fallback)
- All others: empty response

**New dependencies:**
- `simple-dns` — DNS wire format parsing/building
- `scc` — lock-free concurrent hash maps

### Phase 2: Proxy integration

**Modified: `sandbox/supervisor/src/net/proxy.rs`**

`start_proxy` takes `Rc<RefCell<DnsState>>` parameter.

Hostname resolution priority in `handle_connection`:
1. Virtual IP reverse lookup from DnsState (new)
2. TLS SNI extraction (existing fallback)
3. Raw original destination IP (existing fallback)

```rust
let hostname = if let Some(name) = dns.reverse(&orig_ip) {
    name
} else if is_tls {
    tls::extract_sni(...).unwrap_or(orig_host.clone())
} else {
    orig_host.clone()
};
```

### Phase 3: Wiring + init

**Modified: `sandbox/supervisor/src/main.rs`**
- Create `Rc<DnsState>` shared between dns server and proxy
- Call `dns::start(state.clone())` then `start_proxy(..., state)`

**Modified: `sandbox/supervisor/src/net/mod.rs`**
- Add `pub mod dns;`

**Modified: `sandbox/rootfs/init`**
- Before supervisor start: write resolv.conf to container rootfs
  ```sh
  mkdir -p /mnt/bundle/rootfs/etc
  echo "nameserver 10.0.0.1" > /mnt/bundle/rootfs/etc/resolv.conf
  ```
- No iptables changes needed — DNS is UDP to 10.0.0.1 (already
  on lo), goes directly to the supervisor without redirect

## Verification

```bash
# Basic DNS resolution
mise run ez -- nslookup example.com 10.0.0.1

# HTTP through virtual DNS
mise run ez -- wget -qO- http://example.com

# HTTPS through virtual DNS
mise run ez -- wget -qO- https://example.com

# Verify virtual IPs
mise run ez -- sh -c 'nslookup example.com; nslookup github.com'
# Should show different 10.2.x.y addresses

# Repeated query returns same IP
mise run ez -- sh -c 'nslookup example.com; nslookup example.com'
# Should show same 10.2.x.y both times
```
