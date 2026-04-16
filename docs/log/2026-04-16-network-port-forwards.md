# Add network.ports config for port forwarding

### What

New `[network.ports]` config section for declaring host TCP port
forwards into the guest VM, replacing the previous approach of using
`localhost` targets in network allow rules.

### Why

Port forwarding was overloaded onto `network.rules` — you'd write
`[network.rules.localhost] allow = ["localhost:18080"]` to forward a
port. This had a bug: the guest proxy recovers the original destination
IP via `SO_ORIGINAL_DST` and sends `"127.0.0.1"` as the hostname, but
`host_matches("127.0.0.1", "localhost")` does exact string comparison
and returns false, so the connection was denied.

The deeper issue is that port forwarding and network access control are
different concerns. A dedicated config section makes intent clear and
avoids the hostname normalization problem entirely.

Also removed dead kernel cmdline parameters (`airlock.epoch`,
`airlock.shares`, `airlock.host_ports`) — the guest `airlockd` never
reads `/proc/cmdline`; all init parameters are already sent via the
RPC `start()` call.

### Config

```toml
[network.ports.dev-server]
host = [8080, "9000:3000"]  # source:target = host:guest
```

- Plain integer: same port both sides
- `"source:target"` string: host port → guest port mapping
- `PortMapping` uses generic `source`/`target` naming so the same type
  can be reused for guest→host forwarding later

### Design

- `PortForward` config struct with `enabled` flag and `Vec<PortMapping>`
- `PortMapping` newtype with custom serde: accepts integer or
  `"source:target"` string
- `port_forwards_from_config()` replaces `localhost_ports_from_config()`
- Host proxy `resolve_target()` checks port forwards before allow/deny
  rules — forwarded localhost connections always allowed
- Guest iptables setup unchanged (still receives guest ports via RPC)
