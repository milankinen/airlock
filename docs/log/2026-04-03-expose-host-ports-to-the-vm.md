# Expose host ports to the VM

### What

Allow the VM to connect to specific ports on the host machine via the
transparent proxy. Configured via `config.network.host_ports` (default:
`[9999]`).

### Design

The challenge: iptables REDIRECT catches ALL outbound TCP. If a service
runs inside the VM on port 8080, connecting to `localhost:8080` would
loop through the proxy and try to reach the *host's* port 8080 instead.

The fix uses per-port iptables rules:

1. Host ports (from config): `localhost:<port>` → REDIRECT to proxy →
   RPC → host's localhost
2. Other localhost: RETURN (bypass proxy, local services work directly)
3. External traffic: REDIRECT to proxy → RPC → host makes connection

Host ports are passed to the VM via kernel cmdline (`ezpez.host_ports=
9999,8080`). The init script parses them and generates per-port REDIRECT
rules before the general localhost RETURN rule.

On the CLI side, `NetworkProxyImpl` enforces the same allowlist: only
ports listed in `host_ports` are permitted for localhost connections.
This provides defense-in-depth (iptables in VM + RPC filtering in CLI).
