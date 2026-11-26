# Numeric hosts and loopback deny

## Why

* The resolver reads `127.1`, `2130706433`, `0x7f.1` and `0177.0.0.1`
  as IPv4 addresses, but the sandbox host parsed them as names. The
  address rules did not match them, and with no deny range the dial
  still reached the address. A deny of `localhost` did not block `127.1`.
* A deny of `localhost` gave no address range. A name that resolves to
  a loopback address, for example `foo.localhost` or the Debian host
  name (`127.0.1.1`), got past it.
* The IPv4-in-IPv6 unwrap read `::1` as `0.0.0.1` and `::` as `0.0.0.0`.
  A deny of `0.0.0.0/8` (also in the private network pack) then denied
  `::1`, and an exception for `::1` had no effect. The local-use NAT64
  prefix `64:ff9b:1::/48` was not unwrapped.
* A grant request for an allowed name got "already allows", also when
  the name resolves into a denied range and the connection fails.
* `req:hostMatches` raised a Lua error for a pattern that is not valid,
  so a script that worked before failed on each request.

## Design

* The host of a connection parses with the URL host parser (WHATWG),
  which reads the numeric IPv4 forms as the resolver does. An address
  becomes an IP host. The `url` crate was already a dependency. It
  replaces an own parser.
* A host that is neither an IP address nor a valid DNS host name is
  denied under each policy. Valid: ASCII letters, digits, `-` and `_`,
  and the URL parser gives a domain. This covers numeric forms that the
  URL parser refuses but macOS reads as an address (overflow such as
  `0x17f000001`), and scoped addresses such as `fe80::1%en0`.
* Rule patterns use the same name check, so numeric, non-ASCII and `%`
  patterns are a config error. They could never match a guest host.
* A deny of the name `localhost` adds the deny ranges `127.0.0.0/8`
  and `::1/128`. An address pattern such as `127.0.0.1` stays exact, so
  an exception for one loopback address still works.
* The unwrap skips `::` and `::1`, and adds the IPv4-translated form and
  local-use NAT64 in the usual /96 form. Other forms of that prefix are
  not known to the host.
* Deny rules match through the embedded IPv4 address, allow rules and
  other targets do not: a 6to4 address is a different host. Separate
  allow and deny match functions make this clear.
* A grant cannot lift deny ranges or public-only. If a requested name
  passes the rules but its addresses get checked, the reply says that a
  grant cannot allow a denied address. No DNS lookup at request time,
  because a lookup of a requested name can carry data out.
* `req:hostMatches` gives `false` for a pattern that is not valid.

## Testing

* `network/host.rs`: numeric forms, valid and invalid hosts, refused
  patterns.
* `network/target.rs`: IPv6 addresses match IPv4 ranges only through a
  real embedded address.
* `test_policy_rules.rs`: a deny of `localhost` against a real local
  server with numeric forms, loopback deny ranges, and invalid hosts
  under each policy.
* `test_temporary_grant.rs` and `test_middleware_lua.rs`: the grant
  reply text and `hostMatches` with invalid patterns.
