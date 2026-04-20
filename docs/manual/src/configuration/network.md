# Network

The VM has no network interfaces of its own. All TCP traffic from the guest
is routed through a vsock channel back to the host, where airlock evaluates
it against the configured network rules. This gives the host full control
over what the sandbox can reach.

## Policy

The network `policy` controls the overall behavior before rules are evaluated:

```toml
[network]
policy = "deny-by-default"
```

Available policies:

| Policy             | Behavior                                                      |
|--------------------|---------------------------------------------------------------|
| `allow-always`     | Skip rules, allow all connections (default)                   |
| `deny-always`      | Skip rules, deny everything (including port forwards/sockets) |
| `allow-by-default` | Allow unless explicitly denied by a rule                      |
| `deny-by-default`  | Deny unless explicitly allowed by a rule                      |

With `deny-by-default`, only connections matching an explicit `allow` rule
are permitted. This is the recommended starting point for security-sensitive
projects. With `deny-always`, all network access is blocked — including port
forwards and Unix socket forwarding.

## Network rules

Rules are named entries under `[network.rules]`. Each rule defines allow
and/or deny patterns:

```toml
[network.rules.package-registry]
allow = [
    "registry.npmjs.org",
    "registry.yarnpkg.com",
]
```

Patterns support wildcards for both host and port:

```toml
[network.rules.company-services]
allow = [
    "*.prod.example.com", # any subdomain
    "registry.example.com:443", # specific port
    "*:80", # any host on port 80
]
deny = [
    "internal.prod.example.com", # except this one
]
```

Deny patterns are always checked first and win unconditionally, regardless of
allow rules. This makes it safe to use broad wildcards in allow lists while
still blocking specific destinations.

Rules can be disabled without removing them:

```toml
[network.rules.debug-access]
enabled = false
allow = ["*"]
```

### Passthrough

By default, every allowed connection is peeked at to detect TLS and HTTP so
that the traffic can be intercepted and surfaced in the monitor. For
non-HTTP protocols whose first bytes are neither ASCII request lines nor
a TLS `ClientHello`, that detection would deadlock waiting for input the
protocol will never send (Postgres' 8-byte `SSLRequest` is the classic
example).

Mark such rules with `passthrough = true` to skip all detection and relay
the connection as plain TCP:

```toml
[network.rules.database]
allow = ["db.example.com:5432"]
passthrough = true
```

A passthrough target cannot also be covered by middleware — the two are
incompatible, and airlock refuses to start if it finds a rule target that
also appears in any middleware `target` list, naming the offending rule
and middleware.

Port and unix socket forwards are always passthrough: the guest-side
`localhost:<port>` may carry arbitrary traffic to whatever service runs
on the host port, so interception is suppressed automatically.

## Middleware

When you need to do more than just allow or deny connections — for example,
injecting authentication headers or inspecting request paths — you can define
middleware. Middleware is separate from rules and matches connections by its
own `target` patterns. It triggers transparent TLS interception for matching
hosts, letting airlock read and modify HTTP traffic.

```toml
[network.middleware.my-api-auth]
target = ["api.example.com:443"]
env.TOKEN = "${MY_API_KEY}"
script = '''
if not env.TOKEN then
    req:deny()
end
req:setHeader("Authorization", "Bearer " .. env.TOKEN)
'''
```

The `target` field uses the same pattern syntax as rule `allow`/`deny` lists.
Middleware only runs for connections that have been allowed (by policy or rules)
— denied connections never reach middleware.

The `env` table maps names to values expanded from the host environment using
`${VAR}` syntax. Inside the Lua script, these are available as `env.TOKEN`
(or `nil` if the host variable isn't set).

A per-project CA certificate is automatically generated and installed in the
VM's system trust store, so TLS interception is transparent to processes
inside the container — they see valid certificates.

Middleware can be disabled without removing it:

```toml
[network.middleware.my-api-auth]
enabled = false
target = ["api.example.com:443"]
script = '...'
```

For a complete guide to the scripting API — including request/response
inspection, body manipulation, and chaining multiple middleware layers — see
[Network scripting](../advanced/network-scripting.md).

## Unix socket forwarding

Host Unix sockets can be forwarded into the guest container. This is commonly
used for Docker socket access:

```toml
[network.sockets.docker]
host = "/var/run/docker.sock"
```

When the host and guest paths differ, use `"source:target"` syntax
(host path : guest path):

```toml
[network.sockets.docker]
host = "~/.docker/run/docker.sock:/var/run/docker.sock"
```

The socket appears at the specified guest path and connections are relayed
back to the host socket transparently. Like other config entries, socket
forwards can be disabled with `enabled = false`.

## Port forwarding

Port forwards bridge TCP between the host and the guest in either
direction. Each forward is declared under `[network.ports.<group>]` and
every entry uses the same `"host:guest"` string syntax — the **left
side is always the host port, the right side is always the guest
port**, regardless of which direction the forward runs.

A plain integer shorthand (`[5432]`, `[3000]`) means the same port on
both sides.

### Guest → host (`host = [...]`)

Some projects run supporting services on the host — a local PostgreSQL,
a Redis, a dev-mode backend on port 3000 — and the sandboxed process
needs to talk to them. Rather than expose those services to the
network, airlock can forward specific host TCP ports into the VM so
that `localhost:<port>` inside the sandbox transparently reaches the
host service, while everything else on loopback stays confined to the
guest.

```toml
[network.ports.local-services]
host = [5432, 6379]
```

This makes the host's PostgreSQL and Redis available at
`localhost:5432` and `localhost:6379` inside the sandbox. Guest → host
forwards bypass network rules entirely — they're always allowed
regardless of `policy` (except `deny-always`, which blocks everything).

Each entry is either a plain port (same port on both sides) or a
`"host:guest"` string:

```toml
[network.ports.dev]
host = [8080, "9000:3000"]  # guest `localhost:3000` → host port 9000
```

### Host → guest (`guest = [...]`)

The inverse: a service running *inside* the sandbox can be reached
from the host. airlock binds a listener on `127.0.0.1:<host_port>` and
every accepted connection is bridged to `127.0.0.1:<guest_port>`
inside the guest.

```toml
[network.ports.web]
guest = ["5000:4000"]  # host `127.0.0.1:5000` → guest `localhost:4000`
```

Notes:

- **Loopback only.** Listeners bind on `127.0.0.1`; the forward is
  not reachable from the LAN.
- **No rules, no policy.** Host → guest traffic bypasses
  `allow`/`deny`/middleware entirely — the host is trusted, and
  `deny-always` does *not* block reverse forwards.
- **Startup-time bind.** If the host port is already in use the
  sandbox fails to start with a clear error.
- **Host-port collisions are an error.** Two `.guest` entries sharing
  the same host port is rejected at startup.

### Combined example

Both directions can be declared side by side in the same group:

```toml
[network.ports.dev]
host  = ["9000:3000"]   # host :9000 ← guest :3000
guest = ["5000:4000"]   # host :5000 → guest :4000
```

Like other config entries, port forward groups can be disabled with
`enabled = false`.

