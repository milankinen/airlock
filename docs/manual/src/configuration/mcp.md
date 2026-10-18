# MCP servers

> [!WARNING]
> MCP servers are experimental. The configuration and the endpoint
> behavior can change without notice.

> [!CAUTION]
> An MCP server runs on the host, outside the sandbox, with the rights
> of your user. It gives the sandbox access to host resources. Any
> process in the sandbox can call its tools. Add only servers that you
> trust with this access.
>
> The configuration-change prompt of airlock protects the `[mcp.*]`
> entries. airlock asks for your approval when a project configuration
> or a pack adds or changes a server.

airlock can expose stdio MCP servers of the host to the sandbox. Inside
the sandbox, each server is an MCP
[Streamable HTTP](https://modelcontextprotocol.io/specification/2025-06-18/basic/transports#streamable-http)
endpoint below `http://mcp.airlock`.

## Minimal example

To add an MCP server, add a `[mcp.<name>]` block to the `airlock.toml`
of the project. The name is the last part of the endpoint URL. It can
contain letters, digits, `-` and `_`.

This example exposes the mise of the host with
[`mise mcp`](https://mise.jdx.dev/cli/mcp.html):

```toml
[mcp.mise]
command = "mise"
args = ["mcp"]
```

The server is now available inside the sandbox at
`http://mcp.airlock/mise`. Refer to the documentation of your agent for
how to add an HTTP MCP server. A typical `.mcp.json` configuration is:

```json
{
  "mcpServers": {
    "mise": {
      "type": "http",
      "url": "http://mcp.airlock/mise"
    }
  }
}
```

## Full reference

```toml
[mcp.my-server]
enabled = true                 # default true
command = "my-mcp-server"      # required; run on the host
args    = ["--stdio"]          # default empty
timeout = 600                  # idle seconds, default 600, 0 = no limit
max_sessions = 50              # default 50, minimum 1

[mcp.my-server.env]
FOO = "literal"
BAR = "${HOST_VAR}"            # ${VAR} resolved on the host
BAZ = "${OTHER_VAR:default}"
```

### `command`

The program to run on the host. airlock looks it up on the host `$PATH`
unless the path is absolute. The working directory is the project
directory. Required.

### `args`

Arguments for the program. Default: none. Arrays from different
configuration files concatenate.

### `env`

Environment variables for the server process. They add to the host
environment. The values support the same `${VAR}` and `${VAR:default}`
substitution as the top-level [`[env]`](./env.md) section. airlock
resolves them on the host when the sandbox starts. The values never go
into the sandbox. The start fails if a variable has no value and no
default.

### `timeout`

The idle time in seconds after which airlock stops a session. A session
is idle when no message goes in either direction. Default: 600. Set
`timeout = 0` to keep idle sessions until the client ends them.

An open request does not keep a session. If a tool call sends no
message for longer than `timeout`, airlock stops the session. Set a
larger `timeout` for servers with slow tools.

### `max_sessions`

The maximum number of sessions of this server at the same time. Default:
50. The value must be 1 or more. When a client starts one more session,
airlock stops the session with the oldest activity. It also stops a busy
session.

### `enabled`

Set `enabled = false` to disable a server without removing the entry.

## Limits

- airlock supports the Streamable HTTP transport only. It does not
  support the legacy SSE transport.
- airlock does not support resumable streams. A client cannot continue a
  stream after a lost connection.
- airlock does not support JSON-RPC batches. It refuses an array body.
- airlock refuses a request with an `Origin` header other than
  `mcp.airlock`, `localhost` or `127.0.0.1`. It answers HTTP 403.
- A line from the server that is longer than 16 MiB ends the session.
- A message to the server fails when the server does not read its input.
  airlock queues up to 64 messages for each session.
