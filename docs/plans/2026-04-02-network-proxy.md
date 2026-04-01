# Transparent network proxy via vsock RPC

## Context

The VM has no network devices. All container network traffic must
flow through the host CLI via vsock RPC, where it can be filtered,
logged, and have secrets injected. We implement a transparent proxy
in the supervisor that intercepts all outbound TCP, does TLS MITM
for HTTPS inspection, and relays through RPC to the host CLI which
makes the actual connections.

## Architecture

```
Container process
  │ outbound TCP (e.g. curl https://api.example.com)
  │
  ▼ iptables -t nat REDIRECT → supervisor:8080
Supervisor (transparent proxy)
  ├─ Peek first bytes: TLS ClientHello? → extract SNI hostname
  │   ├─ Generate leaf cert for hostname (signed by project CA)
  │   └─ TLS terminate with rustls + generated cert
  ├─ Or plain HTTP → extract Host header via httparse
  │
  ▼ RPC: network_proxy.connect(host, port) → ByteStream
CLI (host, macOS)
  ├─ Apply filtering rules (allow/deny by hostname/IP)
  ├─ Make real TCP connection to destination
  ├─ For TLS: establish real TLS to server
  └─ Relay bytes bidirectionally via ByteStream
```

## Plan

### Phase 1: Kernel + rootfs — enable netfilter

**`sandbox/kernel/config-arm64`:** add:
```
CONFIG_NF_CONNTRACK=y
CONFIG_NF_NAT=y
CONFIG_NF_TABLES=y
CONFIG_NF_TABLES_INET=y
CONFIG_NFT_NAT=y
CONFIG_NFT_REDIR=y
CONFIG_IP_NF_IPTABLES=y
CONFIG_IP_NF_NAT=y
CONFIG_IP_NF_FILTER=y
```

**`sandbox/rootfs/build.sh`:** add `iptables` to apk install

**`sandbox/rootfs/init`:** add iptables redirect after mounts:
```sh
# Redirect all outbound TCP to supervisor proxy (except vsock)
iptables -t nat -A OUTPUT -p tcp -j REDIRECT --to-port 8080
```

**Verify:** Boot VM, `iptables -t nat -L` shows the REDIRECT rule.

### Phase 2: RPC schema — NetworkProxy capability

**`protocol/schema/supervisor.capnp`:**
```capnp
interface Supervisor {
  ping @0 () -> (id :UInt32);
  exec @1 (stdin :ByteStream, pty :PtyConfig, network :NetworkProxy)
    -> (proc :Process);
}

interface NetworkProxy {
  connect @0 (host :Text, port :UInt16, tls :Bool)
    -> (upstream :ByteStream);
}
```

The CLI passes a `NetworkProxy` capability when calling `exec`. The
supervisor uses it to open real connections through the host.

`tls :Bool` tells the CLI whether to establish a TLS connection to
the real server (for HTTPS traffic the supervisor has already
decrypted on the client side).

### Phase 3: CA generation + installation

**`cli/src/project.rs`:** on `ensure()`, generate a CA cert + key if
not present in `project.dir/ca/`:
- Use `rcgen` to create a self-signed root CA
- Store `ca.crt` and `ca.key` as PEM files

**Container CA installation:** Add the CA cert as a VirtioFS-mounted
file. In the OCI config.json, add a bind mount:
- Source: `/mnt/files_ro/<ca_cert>` (hard-linked from project ca/)
- Dest: `/usr/local/share/ca-certificates/ezpez-ca.crt`

The init script or crun prestart runs `update-ca-certificates` to
install it in the system trust store. Or: append the CA cert directly
to `/etc/ssl/certs/ca-certificates.crt` in the container rootfs
during bundle preparation.

### Phase 4: Supervisor — transparent proxy listener

New module: `sandbox/supervisor/src/net/`

**`net/proxy.rs`:** TCP listener on port 8080 in the supervisor:
- Accept connections (these are iptables-redirected)
- Get original destination via `SO_ORIGINAL_DST` getsockopt
- Peek first bytes to detect TLS (0x16 = TLS record)

**`net/tls.rs`:** TLS interception:
- Parse ClientHello to extract SNI hostname
- Generate leaf cert for that hostname using `rcgen`, signed by
  project CA (CA key embedded in supervisor or passed via RPC)
- Accept TLS with `tokio-rustls` using the generated cert
- Returns decrypted async stream

**`net/relay.rs`:** Connection relay:
- Call `network_proxy.connect(host, port, is_tls)` via RPC
- Get back a `ByteStream` (upstream connection)
- Wrap in `InputStream` (AsyncRead)
- `tokio::io::copy_bidirectional` between client stream and upstream

**Integration in `rpc/process.rs`:** before spawning crun, start
the proxy listener. Pass the `NetworkProxy` capability to it.
The proxy runs as a `spawn_local` task alongside the process.

### Phase 5: CLI — NetworkProxy implementation

**New: `cli/src/rpc/network.rs`:**
- Implements `network_proxy::Server`
- `connect(host, port, tls)`:
  - Apply filtering rules (config-based allow/deny)
  - Open real TCP connection via `tokio::net::TcpStream`
  - If `tls`: wrap with `tokio_native_tls` or `tokio-rustls` (client)
  - Return `OutputStream` wrapping the connected stream

**`cli/src/rpc/client.rs`:** update `exec()` to pass the
NetworkProxy capability:
```rust
pub async fn exec(&self, stdin, rows, cols, network) {
    req.get().set_network(network);
    ...
}
```

**`cli/src/main.rs`:** create NetworkProxy and pass to exec:
```rust
let network: network_proxy::Client = capnp_rpc::new_client(NetworkProxyImpl::new(&config));
let shell = client.exec(stdin, rows, cols, network).await?;
```

### Phase 6: DNS resolution

DNS queries go through the proxy too (TCP DNS on port 53). For
UDP DNS, add a separate iptables rule to redirect UDP:53 to a
small DNS-over-TCP forwarder in the supervisor. Or simpler: write
a `/etc/resolv.conf` in the container that points to a supervisor
DNS listener.

For MVP: skip DNS interception. The supervisor resolves hostnames
itself when making proxy connections. Container DNS won't work
(no network), but all HTTP/HTTPS traffic works because the proxy
resolves hostnames on the host side.

## Dependencies

**Supervisor** (`sandbox/supervisor/Cargo.toml`):
- `rcgen` — leaf cert generation
- `rustls` + `tokio-rustls` — TLS termination
- `httparse` — HTTP/1.1 header parsing

**CLI** (`cli/Cargo.toml`):
- `rcgen` — CA generation
- `tokio-rustls` — TLS client connections to real servers

**Protocol** (`protocol/Cargo.toml`): no changes needed

## Verification

1. `mise run build` — kernel rebuilds with netfilter, rootfs has iptables
2. `mise run ez` — boot, `curl http://example.com` works through proxy
3. `curl https://example.com` — TLS intercepted, works with custom CA
4. Hostname filtering: configure deny rule, verify connection rejected
5. `iptables -t nat -L` inside container shows REDIRECT rule
