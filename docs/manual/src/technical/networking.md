# Networking

The VM has no network card that connects to the outside. All network
access goes through a proxy on the host. In the VM, a virtual network
device is the default route. A small TCP/IP stack in the supervisor
receives all outbound TCP connections from this device. This includes
the connections from containers in the sandbox, for example Docker.

The supervisor sends each connection to the host CLI over vsock. The
CLI applies the policy, connects to the real server and relays the
bytes:

```
process in sandbox connects to host:port
  → virtual DNS gives a synthetic IP for the host name
  → the packets go to the virtual network device
  → the supervisor accepts the TCP connection
  → the supervisor finds the host name for the synthetic IP
  → the host CLI checks the policy and connects to the real server
  → the CLI relays the bytes in both directions
```

Network traffic and the other host-guest traffic (terminal, logs,
control) use two different vsock channels. Thus a large download does
not slow down the terminal.

## Flow control

Each connection has its own flow control in both directions. A slow
reader stops only its own connection. Other connections continue.

When one side of a connection closes, the proxy closes the other side
too. A time limit stops a close that does not complete.

## Virtual DNS

The supervisor runs a DNS server in the VM. It does not send queries to
the host. Instead, it gives each host name a synthetic IP address from a
private range and keeps the map between the two.

The proxy sees only the IP address of a connection. With the map, it
finds the host name again. The host name is necessary for the policy and
for TLS. This works for all protocols on TCP. The CLI resolves the real
address of the host name on the host. The synthetic addresses never
leave the VM.

The supervisor keeps the `airlock` domain for its own services, for
example `admin.airlock` and the [MCP servers](./mcp.md) at
`mcp.airlock`. These names point to the VM loopback, and the supervisor
serves them itself. They never leave the VM.

## Policy evaluation

For each connection, the CLI checks the host name and the port against
the [network policy](../configuration/network.md#policy):

- `allow-always` allows all connections. `deny-always` denies all
  connections, also port forwards and socket forwards from the sandbox
  to the host.
- `allow-by-default` and `deny-by-default` use the rules:
  1. If a `deny` pattern matches, the CLI blocks the connection.
  2. If an `allow` pattern matches, the CLI allows the connection.
  3. Else the policy default applies.

The hosts of an enabled [network service](./services.md) are allowed
under all policies except `deny-always`. If the service is not
available, the CLI denies its hosts. Rules from all config files and
packs add together.

## Temporary network grants

A grant gives one command network access that the normal policy does not
allow. The grant has a list of hosts or an
[on-demand rule](../configuration/network.md#rule-editing). The agent
asks for a grant through the [`airlock` MCP server](./mcp.md). The host
checks the request against the config, and you approve it (see
[Approval dialogs](./approvals.md)).

The grant applies only to the connections of that command:

- The command runs in its own network namespace. Its connections have a
  source address that is unique for the grant.
- For connections from this address, the policy uses the grant hosts or
  the rule in addition to the normal rules.
- Deny rules, the `deny-always` policy, the hosts of network services and
  local addresses still block the connection.
- When the command ends, airlock removes the grant and closes its open
  connections.

An approved rule change applies to new connections. Airlock writes it to
the project config file and keeps the other content of the file, with
its comments. If the rule comes from a different config file, the change
goes to the project file as an override.

## TLS interception

Each project has its own CA. The guest adds the CA certificate to the
trust store of the sandbox (see
[CA certificate injection](./mounts.md#ca-certificate-injection)).

The supervisor relays only raw TCP. All TLS work occurs in the host CLI:

1. The CLI reads the first bytes of the connection and checks for a TLS
   handshake.
2. For TLS, the CLI ends the TLS session with a certificate from the
   project CA. It opens a new TLS connection to the real server with the
   same protocol (ALPN) as the client. Thus an HTTP/2 client talks to an
   HTTP/2 server.
3. Other traffic goes through as raw TCP.

Because one process sees both TLS sessions, the protocol of the client
and of the server always agree.

## Lua middleware

[Middleware](../configuration/network.md#middleware) scripts run for
each HTTP request and response of an allowed connection that matches
their targets. This does not depend on the rule that allowed the
connection. The CLI compiles the scripts at start. See
[Network scripting](../advanced/network-scripting.md).

## Secret injection

The guest gets a surrogate for each masked secret. The host keeps the
real value. The surrogate has the same length as the real value.

For hosts that a rule allows to get the secret, the proxy changes the
surrogate to the real value in the request headers. In the response
headers and body, it changes the real value back to the surrogate. The
proxy also finds a value that the server splits across two chunks. It
asks the server for uncompressed answers and refuses compressed answers,
because it cannot search compressed data.

## Localhost port forwarding

For each host port in the config, the supervisor listens on that port on
the VM loopback before user processes start. It sends each connection to
the host port through the proxy. Other loopback traffic stays in the VM.

## Unix socket forwarding

When a process in the sandbox connects to a forwarded socket path, the
supervisor sends the path to the CLI. The CLI finds the host socket for
the path and connects to it. `~` in the sandbox path is the home
directory of the container user. `~` in the host path is the home
directory of the host user.
