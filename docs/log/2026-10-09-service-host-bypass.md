# Close the service host bypass

Review finding #1 (`FINDINGS.md` in faa033a).

## Problem

A service interceptor owned its hosts on port 443 only. Two ways got
past it:

- Another port. `auth.openai.com:8443` was a normal host for the policy.
  Under `allow-always` or an allow rule, the proxy relayed it with no
  interceptor.
- Another host with the service host in `Host` / `:authority`. On a
  connection that no service owns, the header went upstream as the guest
  sent it. A CDN that routes on `Host` sends the request to the service.

Scenario: the user approves a device code in the host browser. The guest
polls the token host on one of these routes and gets the real
`authorization_code`, then exchanges it for real tokens. The Anthropic
code swap had the same gap. The hosts of a service that cannot run (no
token store) had the same port gap.

## Change

- `Network::resolve_target`: a host that a service owns (on any port), or
  that belongs to a service that cannot run, is denied on each port that
  no interceptor handles. This applies under every policy, and it
  replaces the old exact-port check of the unavailable targets. Port
  forwards to localhost are not affected.
- `ResolvedTarget::service_hosts`: the canonical hosts of all services.
  The HTTP relay refuses a request with 421 Misdirected Request if the
  connection has no interceptor and the URI host or a `Host` header names
  a service host. The check is in the innermost send step, after Lua
  middleware, so a script that changes `Host` cannot get past it. A
  `Host` value that does not parse counts as a match (fail closed),
  because the upstream can read it in a different way.
- Plain HTTP to an owned host: the proxy clears the interceptor for
  non-TLS streams. Thus the new check refuses these requests too (421),
  and no request bytes go upstream. Before, they went upstream raw with
  no swap. This also fixes the plain-HTTP part of finding #8.

## Alternatives

- Compare `Host` with the connect target for each request: this breaks
  valid requests to an IP literal with a name in `Host` (for example
  `curl --resolve`). Only the service hosts need the protection.

## Not covered

- Passthrough rules: the guest does its own TLS and can send any SNI.
  A passthrough rule for a CDN address that also serves a service host
  can still reach the service. Passthrough is an explicit user choice
  for non-HTTP protocols.

## Tests

- `test_service_hosts.rs`: an owned host on port 8443 is denied under an
  allow-all rule. On a connection to another allowed host, `Host` values
  `auth.svc.test`, `AUTH.svc.test.` and `auth.svc.test:8443` get 421 and
  do not reach the upstream. A request with the real host gets there.
- The unavailable-service test now expects port 80 to be denied. The
  plain-HTTP test now expects 421 and no request bytes upstream.
