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
