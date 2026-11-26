# IP addresses in network rules

## Why

* The rules compared hosts as text, but one IP address has many text
  forms. `deny = ["169.254.169.254"]` did not block
  `::ffff:169.254.169.254`, and the guest picks the form.
* Patterns could not cover a range. `10.*` and `10.0.0.0/8` loaded with
  no error and matched nothing, so a deny rule with them blocked nothing.
* Under `allow-by-default` the sandbox reaches the host's LAN, a VPN and
  cloud metadata, and no config could block a range.

## Design

* Each rule target parses one time to a typed host pattern: any,
  suffix, canonical name or IP net. A single address is a /32 or /128
  net. The host of a connection parses to a name or a canonical IP
  address. A name matches only name patterns, and an address only nets.
  The localhost aliases stay equal, as before.
* A pattern that can never match is a config error: a bad range, a `*`
  that is not the full host or a `*.` prefix, and a name with an
  all-numeric last label such as `127.1`. The resolver reads such names
  as addresses.
* Phase 1 (connect) decides with no DNS lookup, because the lookup of a
  denied name can carry data out.
* Phase 2 (relay task, before the upstream connect): for an allowed
  name with deny nets or public-only, resolve the name one time and drop
  the denied addresses. With no address left, the connection gets the
  normal deny with a report. The dial uses the kept addresses, so DNS
  rebinding cannot get past the check. An exact IP deny now also covers
  names, because it is a net.
* Deny nets also match the IPv4 address that an IPv6 address embeds
  (NAT64, 6to4, IPv4-compatible).
* Private addresses are a pack (`private-network`), not network code.
  One list of private ranges serves the public-only check and the pack.
  Packs get two Lua helpers: `ip.private_ranges()` and
  `ip.exclude(ranges, except)`. A deny always wins, so the pack removes
  the `except` addresses from its deny ranges.
* Grant requests and agent rule edits still refuse addresses that are
  not public and ranges in `allow`.

## Testing

* `network/host.rs`: matching by name and address, other address
  forms, refused patterns.
* `test_policy_rules.rs`: each form of a denied address, deny nets for
  names, and a real local server that denied names and addresses never
  reach, also through the pack.
* `test_builtin_packs.rs`: the pack in both modes with exceptions.
