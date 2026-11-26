# Private network

The `private-network` pack controls access from the sandbox to the
private addresses, for example the local network, a VPN and cloud
metadata. The sandbox connects through the host. Thus without the pack,
the private addresses follow the same network rules as the public
addresses.

```toml
[packs]
private-network = { version = "1", args = { mode = "deny" } }
```

| Arg      | Default | Description                                                    |
|----------|---------|----------------------------------------------------------------|
| `mode`   | `deny`  | `deny` blocks the private addresses. `allow` allows them       |
| `except` | `none`  | IP addresses and ranges, comma-separated, that get the opposite of `mode` |

The private addresses are all IPv4 and IPv6 ranges that are not public.

A deny also blocks the host names that resolve to a denied address. To
reach a server by its name, add the address of the server to `except`.
For more about names and addresses, see
[IP addresses and ranges](../configuration/network.md#ip-addresses-and-ranges).

## Examples

Block the private addresses, but allow one server and a VPN range:

```toml
[packs]
private-network = { version = "1", args = { except = "192.168.1.10, 10.8.0.0/16" } }
```

Allow the private addresses under `deny-by-default`, but block the
router:

```toml
[packs]
private-network = { version = "1", args = { mode = "allow", except = "192.168.1.1" } }
```
