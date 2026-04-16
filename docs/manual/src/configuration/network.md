# Network

The VM has no network interfaces of its own. All TCP traffic from the guest
is routed through a vsock channel back to the host, where airlock evaluates
it against the configured network rules. This gives the host full control
over what the sandbox can reach.

## Default mode

By default, all outbound connections are allowed (passthrough, no inspection).
To lock things down, set the default mode to `deny`:

```toml
[network]
default_mode = "deny"
```

With `deny` as the default, only connections matching an explicit `allow` rule
are permitted. This is the recommended starting point for security-sensitive
projects.

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
    "*.prod.example.com",          # any subdomain
    "registry.example.com:443",    # specific port
    "*:80",                        # any host on port 80
]
deny = [
    "internal.prod.example.com",   # except this one
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

## Middleware

When you need to do more than just allow or deny connections — for example,
injecting authentication headers or inspecting request paths — you can attach
Lua middleware to a rule. Middleware triggers transparent TLS interception for
the matching hosts, letting airlock read and modify HTTP traffic.

```toml
[network.rules.my-api]
allow = ["api.example.com:443"]

[[network.rules.my-api.middleware]]
env.TOKEN = "${MY_API_KEY}"
script = '''
if not env.TOKEN then
    req:deny()
end
req:setHeader("Authorization", "Bearer " .. env.TOKEN)
'''
```

The `env` table maps names to values expanded from the host environment using
`${VAR}` syntax. Inside the Lua script, these are available as `env.TOKEN`
(or `nil` if the host variable isn't set).

A per-project CA certificate is automatically generated and installed in the
VM's system trust store, so TLS interception is transparent to processes
inside the container — they see valid certificates.

For a complete guide to the scripting API — including request/response
inspection, body manipulation, and chaining multiple middleware layers — see
[Network Scripting](../advanced/network-scripting.md).

## Unix socket forwarding

Host Unix sockets can be forwarded into the guest container. This is commonly
used for Docker socket access:

```toml
[network.sockets.docker]
host = "/var/run/docker.sock"
guest = "/var/run/docker.sock"
```

The socket appears at the specified guest path and connections are relayed
back to the host socket transparently. Like other config entries, socket
forwards can be disabled with `enabled = false`.

## Port forwarding

Host TCP ports can be forwarded into the VM so that the guest can reach host
services transparently. Forwarded ports are configured under `[network.ports]`:

```toml
[network.ports.local-services]
host = [5432, 6379]
```

This makes the host's PostgreSQL and Redis available at `localhost:5432` and
`localhost:6379` inside the sandbox. Port forwarding bypasses network rules
entirely — forwarded ports are always allowed regardless of `default_mode`.

Each entry in `host` is either a plain port number (same port on both sides)
or a `"source:target"` string for port remapping (host port → guest port):

```toml
[network.ports.dev]
host = [8080, "9000:3000"]  # host 9000 → guest 3000
```

Like other config entries, port forward groups can be disabled with
`enabled = false`.
