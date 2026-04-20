# Network scripting

Middleware entries under `[network.middleware]` trigger transparent TLS
interception on matching connections. This gives your Lua scripts access to
the full HTTP request and response, letting you do things like inject
credentials, enforce path-level access control, or rewrite payloads.

If you haven't read the [Network](../configuration/network.md) chapter yet,
start there — it covers the rule system and policy that middleware builds on.

## How it works

Each middleware entry is a named Lua script with `target` patterns that
determine which connections it applies to. The script receives a `req` object
representing the intercepted HTTP request.

```toml
[network.middleware.add-header]
target = ["api.example.com:443"]
script = '''
req:setHeader("X-Custom", "added-by-airlock")
'''
```

If the script doesn't explicitly block or forward the request, airlock
forwards it automatically after the script finishes — with any modifications
you've applied.

## Environment variables

Middleware can reference host environment variables through the `env` table.
Define the mapping in the middleware config:

```toml
[network.middleware.api-auth]
target = ["api.example.com:443"]
env.TOKEN = "${MY_API_KEY}"
script = '''
req:setHeader("Authorization", "Bearer " .. env.TOKEN)
'''
```

The `${VAR}` syntax reads from the host environment first and the
[secret vault](../secrets.md) as fallback. If a referenced name resolves
in neither, `airlock start` aborts with an error — middleware never runs
with silently-missing inputs, so scripts can treat every declared entry
as present.

## TLS interception

A per-project CA certificate is automatically generated and installed in the
VM's system trust store the first time you start a sandbox. Processes inside
the container see valid certificates for intercepted connections — no manual
trust configuration is needed.

All allowed TLS connections are intercepted so requests are visible in the
Monitor tab, regardless of whether a middleware script matches. Middleware
runs only for connections that match its target; connections with no matching
middleware are still MITM-decrypted but pass through unmodified.

## Request API

The `req` object is available in every middleware script:

| Field / Method | Description |
|---|---|
| `req.method` | HTTP method (`"GET"`, `"POST"`, etc.) |
| `req.path` | URL path (readable and writable) |
| `req.host` | Host header |
| `req.headers` | Full headers table (readable and writable) |
| `req:header(name)` | Read a single header value |
| `req:setHeader(name, value)` | Set or overwrite a header |
| `req:hostMatches(pattern)` | Match host against a wildcard pattern |
| `req:body()` | Read the request body (returns a Body object) |
| `req:setBody(value)` | Replace the body (string, table, Body, or nil) |
| `req:deny()` | Block the request with a 403 response |
| `req:send()` | Forward the request and return the response |
| `log(msg)` | Write to the supervisor debug log |

### Blocking a request

Call `req:deny()` to immediately block the request with a 403 response:

```lua
if req.path:find("^/admin") then
    req:deny()
end
```

### Forwarding and inspecting the response

Call `req:send()` to forward the request to the upstream server. This returns
a response object that you can inspect and modify before it reaches the
client:

```lua
local res = req:send()
if res.status == 200 then
    log("request succeeded: " .. req.path)
end
```

If you don't call `req:send()` or `req:deny()`, the request is forwarded
automatically when the script finishes — but you won't get access to the
response.

## Response API

The response object returned by `req:send()` has a similar interface:

| Field / Method | Description |
|---|---|
| `res.status` | HTTP status code (readable and writable) |
| `res.headers` | Full headers table (readable and writable) |
| `res:header(name)` | Read a single header value |
| `res:setHeader(name, value)` | Set or overwrite a header |
| `res:body()` | Read the response body (returns a Body object) |
| `res:setBody(value)` | Replace the body (string, table, Body, or nil) |

## Body objects

Both `req:body()` and `res:body()` return a Body object with these methods:

| Method | Description |
|---|---|
| `body:text()` | Raw bytes as a Lua string |
| `body:json()` | Parse as JSON, return a Lua table |
| `body:len()` or `#body` | Byte length |

When you call `req:setBody()` or `res:setBody()` with a Lua table, it's
serialized as JSON automatically. The `Content-Length` header is updated to
match the new body size.

**Performance note:** request and response bodies are streamed lazily by
default. Calling `req:body()` or `res:body()` reads the entire body into
memory. For most API traffic this is fine, but be careful with endpoints
that transfer large payloads — a multi-gigabyte upload or download will be
fully materialized in the proxy's memory and could cause an out-of-memory
crash. If you only need to inspect headers or the request path, avoid
calling `body()` altogether.

## Chaining middleware

Multiple middleware entries with overlapping `target` patterns all apply to
matching connections. They run in order, and each one sees the modifications
made by the previous:

```toml
[network.middleware.add-id]
target = ["api.example.com:443"]
script = '''
req:setHeader("X-Request-ID", "abc123")
'''

[network.middleware.log-id]
target = ["api.example.com:443"]
script = '''
-- this script sees the X-Request-ID header set above
log("request id: " .. req:header("X-Request-ID"))
'''
```

If any script calls `req:deny()`, the chain stops and the request is blocked.

## Examples

### Path-based access control

Allow GitHub API requests only to specific endpoints:

```toml
[network.rules.github]
allow = ["api.github.com:443"]

[network.middleware.github-paths]
target = ["api.github.com:443"]
script = '''
local p = req.path
if not (p == "/user" or p:find("^/repos/myorg/")) then
    req:deny()
end
'''
```

### Logging request bodies

```toml
[network.middleware.log-posts]
target = ["api.example.com:443"]
script = '''
if req.method == "POST" then
    local b = req:body()
    log("POST " .. req.path .. " (" .. b:len() .. " bytes)")
end
'''
```

### Modifying a response

```toml
[network.middleware.sandbox-header]
target = ["api.example.com:443"]
script = '''
local res = req:send()
res:setHeader("X-Sandbox", "true")
'''
```

## Sandbox restrictions

Middleware scripts run in a restricted Lua environment. The following standard
libraries are disabled: `os`, `io`, `debug`, `require`, `load`, `loadfile`,
and `dofile`. There is no way to access the host filesystem or execute
external processes from a script.

Each request has an instruction limit of 1,000,000 operations. If a script
exceeds this limit, the request fails with an error. This prevents runaway
scripts from blocking the network proxy.
