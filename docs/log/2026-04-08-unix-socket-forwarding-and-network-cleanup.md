# Unix socket forwarding and network connection cleanup

## Socket forwarding

Added Unix socket forwarding from host to guest via the existing
NetworkProxy RPC. Use case: mounting `/var/run/docker.sock` so
containers can use the host Docker daemon.

### Architecture

Extended `NetworkProxy.connect()` with a union target — either TCP
(host:port) or socket (path). The supervisor creates a Unix socket
listener inside the container rootfs (on `/mnt/disk/sockets/`),
bind-mounted by crun. When a container process connects, the supervisor
calls the host via RPC, which connects to the real socket and relays
bidirectionally.

### Config

```toml
[network.sockets.docker]
host = "/var/run/docker.sock"
guest = "/var/run/docker.sock"
```

### Changes

- RPC schema: `ConnectTarget` union (tcp/socket), `SocketForward` struct
- Config: `network.sockets` map with `SocketForward` entries
- Host: socket connection handler in `server.rs` (same relay pattern)
- Guest: `net/socket.rs` — Unix listener + RPC relay per socket
- OCI config: bind-mount socket files from `/mnt/disk/sockets/`

## Network connection cleanup

Audited and fixed connection lifecycle across all relay paths:

- **`tcp::relay`**: changed from `join!` to `select!` — when either
  direction closes, both sides shut down immediately
- **`proxy.rs` relay**: same `select!` + explicit `close_request()`
- **`socket.rs` relay**: same pattern
- **`RpcTransport::poll_shutdown`**: now sends `close_request()` RPC
  so the remote ChannelSink drops its sender
- **Guest `ChannelSink::send`**: returns error after close (was silent)
- **Connect timeouts**: TCP (10s), TLS (10s), socket (5s) — constants
  in `cli/src/constants.rs`
