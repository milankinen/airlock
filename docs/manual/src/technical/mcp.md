# MCP

The supervisor in the VM serves all MCP endpoints of the sandbox. One
local HTTP server on the guest loopback (port 80) serves them. The
virtual DNS resolves all `*.airlock` names to this address (see
[Networking](./networking.md#virtual-dns)). The traffic never goes
through the TCP proxy and never leaves the VM. The server picks the
endpoints by the host name of the request:

| Host name         | Endpoints                                        |
|-------------------|--------------------------------------------------|
| `admin.airlock`   | Admin service, with the `airlock` MCP server     |
| `mcp.airlock`     | MCP servers of the config, `/<name>`             |
| `acp.mcp.airlock` | MCP servers of editor sessions, `/<key>/<name>`  |

All endpoints use the MCP Streamable HTTP transport. They refuse
requests from web pages of other origins. This is a guard against DNS
rebinding.

## Admin MCP server

The `airlock` MCP server at `http://admin.airlock/mcp` gives the agent
tools for the network policy. It is stateless and sends JSON responses.
It has no MCP sessions. It accepts only the `Host` names
`admin.airlock`, `localhost` and `127.0.0.1`.

The server offers only the tools of the features that the host enables
at boot:

| Tool                  | Offered when                               |
|-----------------------|--------------------------------------------|
| `execute`             | `network.grants.enabled = true`            |
| `show_rules`          | `network.rule_editing` is not `"none"`     |
| `upsert_rule`         | `network.rule_editing` is not `"none"`     |
| `list_manual_entries` | Always                                     |
| `get_manual_entry`    | Always                                     |

The manual tools give the Markdown pages of this manual. The build puts
the pages into the supervisor. The server also gives the agent a text
with instructions. The text describes only the enabled tools. For the
meaning of grants, on-demand rules and rule changes, see
[Temporary network grants](./networking.md#temporary-network-grants).

The [agent packs](../packs/agents.md) `claude@2`, `codex@2` and
`copilot@2` connect the agent to this server. They also give the agent
the tool instructions at the start of each session. Hooks tell the agent
when the network policy blocked a connection during a tool call. Codex
runs shell commands in its own sandbox. That sandbox has no network
unless you start Codex with `--yolo`. Without `--yolo`, airlock sees no
denied connections.

### Request flow

The tools do not decide. The host CLI decides. The supervisor sends each
request to the host over the network channel (see [RPC](./rpc.md)):

1. The tool checks its arguments. For `execute`, it refuses environment
   keys that change program start, code loading, certificates or
   proxies, for example `PATH` and `LD_*`.
2. The supervisor sends the request to the host. The host checks it
   against the config and refuses a request that the config does not
   permit. It does not ask the user then.
3. The host asks the user (see [Approvals](./approvals.md)). The answer
   can take minutes. If the MCP client disconnects, the supervisor drops
   the request, and the host removes the approval request.
4. `show_rules` and `upsert_rule` return the result of the host.
   `execute` continues as below.

An approved `execute` gets a grant proxy from the host. The grant proxy
allows the approved hosts or the rule, in addition to the normal policy.
The supervisor then:

1. Makes a network namespace for the grant. A virtual cable connects it
   to the main network of the VM, with a source address that is unique
   for the grant. The TCP proxy sends the connections from this address
   through the grant proxy. The main network refuses local packets that
   use a grant address as source. Thus other processes cannot use the
   grant without root rights.
2. Runs the command with `sh -c` in the namespace, as the sandbox user.
   The command gets the sandbox environment and the CA variables.
3. Stops the command when the time limit of the grant expires, when the
   MCP client disconnects, or when the user stops the grant on the host.
4. Reads the grant log: the hosts that the command connected to, and if
   the network blocked them.
5. Removes the namespace and releases the grant. The host then closes
   the open connections of the grant.

The result has the exit code, the last 64 KiB of stdout and of stderr,
the hosts, and a flag that tells if the user stopped the command.

## Host MCP servers

The servers of `[mcp.<name>]` (see [MCP servers](../configuration/mcp.md))
run on the host, with the rights of the user. The guest relays the
JSON-RPC messages without change. The host sends them to the stdin of
the server program and reads the replies from its stdout, one message on
each line.

The boot config gives the guest the names of the enabled servers and one
host capability that starts them. The guest cannot start other programs.
Commands, arguments and environment values stay on the host. The host
resolves the `${VAR}` values and never sends them to the guest.

### Sessions

1. An `initialize` request without a session ID opens a session. The
   guest makes a random session ID and asks the host to start a new
   process of the server. The working directory is the project
   directory.
2. Each later request gets its reply as an event stream. The stream also
   carries the server messages that arrive while the request is open.
3. A `GET` opens one more stream for server messages that arrive while no
   request is open. The guest keeps up to 256 of these messages. Then it
   drops the oldest.
4. The session ends when the client sends `DELETE`, when the server
   process stops, or when the host stops the session.

Each session has its own process. Thus the sessions stay apart, and
the request IDs do not change. The server program writes its stderr to
the log. When a session ends, the host stops the process and all
processes that it started.

The host stops a session after the idle `timeout`, and the session with
the oldest activity when a new session exceeds `max_sessions`.

### Editor sessions

With `--proxy-mcp-servers`, [`airlock acp`](../tips/editor-acp.md#editor-mcp-servers)
adds the MCP servers of the editor at run time. The editor can give
local (stdio) servers and remote servers (HTTP and SSE). The host runs
the local servers and relays the remote servers. URLs and headers also
stay on the host.

The servers of one editor session get a random key of 16 hex characters.
The route is `http://acp.mcp.airlock/<key>/<name>`. Thus the servers of
different editor sessions stay apart. The host adds each route with
`addMcpServer` and keeps the grant that the call returns. The route ends
when the host releases or drops the grant. This also ends the open
sessions of the route. The host releases the grants when the editor
closes or loads the session again, and when the `airlock acp` process
stops. The guest accepts at most 256 run-time routes.

### Security notes

- Any process in the sandbox can call a host server. The sandbox has no
  per-process access control for MCP.
- A host server acts outside the sandbox. The network policy does not
  apply to it.
- The config-change prompt protects the `[mcp.*]` entries. A project
  config or a pack cannot add a server without your approval.
