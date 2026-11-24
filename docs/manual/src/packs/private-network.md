# Private network

The `private-network` pack blocks the local network, a VPN, cloud
metadata and the other private addresses. The sandbox connects through
the host. Without the pack, the sandbox can reach all addresses that the
host can reach.

```toml
[packs]
private-network = { version = "1", args = { except = "192.168.1.10, 10.8.0.0/16" } }
```

| Arg      | Default | Description                                                    |
|----------|---------|----------------------------------------------------------------|
| `mode`   | `deny`  | `deny` blocks the private addresses. `allow` allows them, for example under `deny-by-default` |
| `except` | `none`  | IP addresses and ranges, comma-separated, that get the opposite of `mode` |

The private addresses are the loopback, private, link-local, shared
(CGNAT), multicast and reserved ranges, in IPv4 and IPv6.

With `deny`, airlock also blocks the host names that resolve to a
private address. To reach a server on a VPN by its name, add the address
or the range of the server to `except`.

The pack adds the network rule `private-network`. It does not change
port forwards. The `allow-always` policy skips the rule, as it skips all
deny rules.
