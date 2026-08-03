# Parse IPv6 literals in network rule targets

## Symptom

A network rule targeting an IPv6 literal never matched. Under
`allow-by-default`, a deny rule like `::1` (or a public IPv6 address) failed to
block that destination — the guest could still reach it (fail-open). IPv6
allow rules failed closed instead.

## Root cause

`parse_target` split `host[:port]` with `rsplit_once(':')`, which mangles IPv6
literals: `::1` parsed to host `":"` / port `"1"`, and `2001:db8::1` to host
`"2001:db8:"` / port `"1"`. The resulting target matched nothing.

## Fix

`parse_target` now recognizes IPv6 forms before the colon split:

- `[::1]` / `[::1]:443` — bracketed literal, port taken after the `]`.
- a bare literal with more than one `:` (`2001:db8::1`) is host-only (there is
  no unbracketed `host:port` form for IPv6).
- hostnames and IPv4 keep the previous `host[:port]` behavior.

## Tests

`parse_target_handles_hostnames_and_ipv4` and
`parse_target_handles_ipv6_literals` cover bare and bracketed IPv6 (with and
without a port) alongside the hostname/IPv4 cases.
