# Network

The VM has no network interfaces of its own. All TCP traffic from the guest
routes back to the host. There, airlock evaluates
it against the configured network rules. This gives the host full control
over what the sandbox can reach.

## Policy

The network `policy` controls the overall behaviour before airlock evaluates
rules:

```toml
[network]
policy = "deny-by-default"
```

Available policies:

| Policy             | Behavior                                                              |
|--------------------|-----------------------------------------------------------------------|
| `allow-always`     | Skip rules, allow all connections (default)                           |
| `deny-always`      | Skip rules, deny everything (including guest → host forwards/sockets) |
| `allow-by-default` | Allow unless explicitly denied by a rule                              |
| `deny-by-default`  | Deny unless explicitly allowed by a rule                              |

With `deny-by-default`, airlock permits only connections that match an
explicit `allow` rule. This is the recommended starting point for
security-sensitive projects. With `deny-always`, airlock blocks all network
access from the guest — including guest → host port forwards and Unix
socket forwarding. Host → guest reverse forwards are the exception (see
[Port forwarding](#port-forwarding)).

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

The port part must be a number or `*` (or left out to match all ports).

airlock always checks deny patterns first, and they win unconditionally,
regardless of allow rules. This makes it safe to use broad wildcards in
allow lists while still blocking specific destinations.

Host matching is case-insensitive and ignores a trailing dot. A rule for
`secret.example.com` also matches `SECRET.example.com` and
`secret.example.com.` — a destination cannot evade a deny rule with a
change of letter case.

You can disable rules without removing them:

```toml
[network.rules.debug-access]
enabled = false
allow = ["*"]
```

### Passthrough

By default, airlock peeks at the first bytes of every allowed connection to
detect TLS and HTTP. This lets it intercept the traffic and show it in the
monitor. For non-HTTP protocols whose first bytes are neither ASCII request
lines nor a TLS `ClientHello`, that detection would deadlock. It would wait
for input the protocol will never send (Postgres' 8-byte `SSLRequest` is
the classic example).

Mark such rules with `passthrough = true` to skip all detection and relay
the connection as plain TCP:

```toml
[network.rules.database]
allow = ["db.example.com:5432"]
passthrough = true
```

A passthrough target cannot also be covered by middleware, by an
injecting rule or by a [network service](#network-services) — all need
interception. airlock refuses to start and
names the conflict.

Port and unix socket forwards are always passthrough: the guest-side
`localhost:<port>` may carry arbitrary traffic to whatever service runs
on the host port, so airlock suppresses interception automatically.

### Injecting masked secrets

`inject` lists [masked](env.md#masking) variables. For HTTP traffic to the
rule's `allow` targets, airlock replaces the surrogate with the real value
in request headers. It also replaces the real value with the surrogate in
response headers and response bodies.

```toml
[env]
CLAUDE_CODE_OAUTH_TOKEN = { value = "${CLAUDE_CODE_OAUTH_TOKEN}", mask = true }

[network.rules.claude-code]
inject = ["CLAUDE_CODE_OAUTH_TOKEN"]
allow = ["api.anthropic.com:443", "claude.ai:443"]
```

The sandboxed program sends `Authorization: Bearer $CLAUDE_CODE_OAUTH_TOKEN`
as it would on the host. airlock inserts the real token at the host boundary.

- Names must be `[env]` entries with `mask = true`.
- Values must be at least 8 characters and valid in an HTTP header.
- Injecting rules cannot be `passthrough`.
- In requests, airlock rewrites only header values — every header, every
  occurrence. It does not rewrite header names, paths, or request bodies.
- In responses, airlock rewrites header values and the body. It finds a
  real value also when the server splits it across two body chunks.
- airlock asks the server for an uncompressed response
  (`Accept-Encoding: identity`). If the server still sends a compressed
  body, airlock replaces the response with HTTP 502. airlock cannot find
  a real value in compressed data.
- airlock does not find an encoded real value, for example in base64 or
  URL encoding. It also does not check WebSocket data after an upgrade.
- airlock unmasks request headers before [middleware](#middleware) runs
  and masks the response after it, so scripts see real values. The
  monitor shows surrogates.

## Network services

A network service lets a coding agent sign in inside the sandbox, but
keeps the real tokens on the host. The [agent packs](../packs/agents.md)
turn on their service. You can also set it yourself:

```toml
[network.services]
anthropic = true
```

| Service     | Agent       | Hosts                                       |
|-------------|-------------|---------------------------------------------|
| `anthropic` | Claude Code | `platform.claude.com`, `api.anthropic.com`  |
| `openai`    | Codex       | `auth.openai.com`, `chatgpt.com`            |

When a service is on:

- airlock allows the hosts of the service, also with `deny-by-default`.
  `deny-always` and `deny` patterns still block them.
- The sign-in page opens in the browser of the host. If your `[env]` sets
  `BROWSER`, airlock does not change it.
- airlock keeps the real tokens on the host and gives the agent
  surrogates. On the API hosts, airlock replaces the surrogate with the
  real token.
- An API request must use a surrogate of the service or an
  [injected](#injecting-masked-secrets) masked secret. airlock answers
  other credentials with HTTP 401.
- airlock refuses an answer that contains a real token (HTTP 502).
- All sandboxes share the sign-ins. A logout in one sandbox signs out all
  sandboxes. `airlock show` lists the sign-ins.

airlock encrypts the tokens with a key from the [secret vault](../secrets.md).
If the vault is `disabled` or airlock cannot read the key, the service is
not available. Then airlock blocks its hosts and shows a warning. To let
the agent sign in without airlock, turn off the service:

```toml
[network.services]
anthropic = false
```

Then the real tokens are in the sandbox, and you must allow the hosts with
your own rules.

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

airlock automatically generates a per-project CA certificate and installs
it in the VM's system trust store. TLS interception is therefore
transparent to processes inside the container — they see valid
certificates.

You can disable middleware without removing it:

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

airlock can forward host Unix sockets into the guest container. A common
use is Docker socket access:

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

The socket appears at the specified guest path. airlock relays connections
back to the host socket transparently. Like other config entries, you can
disable socket forwards with `enabled = false`.

## Port forwarding

Port forwards bridge TCP between the host and the guest in either
direction. You declare each forward under `[network.ports.<group>]`, and
every entry uses the same `"host:guest"` string syntax — the **left
side is always the host port, the right side is always the guest
port**, regardless of which direction the forward runs.

A plain integer shorthand (`[5432]`, `[3000]`) means the same port on
both sides.

### Guest → host (`host = [...]`)

Some projects run supporting services on the host — a local PostgreSQL,
a Redis, a dev-mode backend on port 3000 — and the sandboxed process
needs to reach them. Rather than expose those services to the network,
airlock can forward specific host TCP ports into the VM.
`localhost:<port>` inside the sandbox then transparently reaches the
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

The inverse: the host can reach a service running *inside* the
sandbox. airlock binds a listener on `127.0.0.1:<host_port>` and
bridges every accepted connection to `127.0.0.1:<guest_port>`
inside the guest.

```toml
[network.ports.web]
guest = ["5000:4000"]  # host `127.0.0.1:5000` → guest `localhost:4000`
```

Notes:

- **Loopback only.** Listeners bind on `127.0.0.1`. The forward is
  not reachable from the LAN.
- **No rules, no policy.** Host → guest traffic bypasses
  `allow`/`deny`/middleware entirely — the host is trusted, and
  `deny-always` does *not* block reverse forwards.
- **Startup-time bind.** If the host port is already in use the
  sandbox fails to start with a clear error.
- **Host-port collisions are an error.** airlock rejects two `.guest`
  entries that share the same host port at startup.

### Combined example

You can declare both directions side by side in the same group:

```toml
[network.ports.dev]
host = ["9000:3000"]   # host :9000 ← guest :3000
guest = ["5000:4000"]   # host :5000 → guest :4000
```

Like other config entries, you can disable port forward groups with
`enabled = false`.

