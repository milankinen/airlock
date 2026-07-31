# Base middleware host checks on the connect target, not the Host header

## Symptom

Host-based decisions in network middleware scripts could be bypassed by
spoofing the HTTP `Host` header. With a broad allow (`*.example.com`) and a
middleware rule like `if req:hostMatches("internal.example.com") then
req:deny() end`, a guest could connect to `internal.example.com` (allowed by
the wildcard) but send `Host: public.example.com`; `hostMatches` saw
`public.example.com`, did not deny, and the request still went to the real
upstream `internal.example.com` — the control was defeated.

## Root cause

`req.host` and `req:hostMatches` derived the host from `p.uri.host()` falling
back to the request `Host` header — both fully guest-controlled — instead of
the connection's authenticated destination, which is fixed at connect time and
already known to the proxy (`ResolvedTarget.host`).

## Fix

Thread the authenticated connect host into the middleware runner and its Lua
`State`, and have `req.host` and `req:hostMatches` report *that* host rather
than the request URI / `Host` header. Scripts that genuinely want the raw
client-sent header can still read it with `req:header("host")`.

## Docs

`docs/manual/src/advanced/network-scripting.md` updated: `req.host` /
`req:hostMatches` are documented as the connect target, and `req:header("host")`
as the way to read the raw header.

## Tests

- `spoofed_host_header_does_not_trigger_host_rule` — connecting to localhost
  with `Host: evil.com` no longer trips a rule denying `evil.com` (it did
  before the fix).
- `host_rule_matches_connect_target` — a rule denying the real connect target
  (`127.0.0.1`) does fire, proving `hostMatches` sees the authenticated host.
