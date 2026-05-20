# Recursive wildcard domain matching for network filtering

## Background

Airlock intercepts all outbound TCP from the guest and evaluates each
connection against configured allow/deny rules and middleware targets. Both
use `*.suffix` wildcard patterns — e.g. `*.github.com` to allow all GitHub
subdomains.

## Problem

The matcher enforced strict RFC 6125 (TLS certificate) wildcard rules, where
`*.example.com` matches only a single DNS label:
- ✅ `*.github.com` matched `api.github.com`
- ❌ `*.github.com` rejected `api.business.github.com`

Users writing rules for real-world services (GitHub, AWS, Google APIs) expect
all subdomains at any depth to be covered by one wildcard entry. RFC 6125 is
designed for TLS certificate validation, not network filtering.

## Decision

Change wildcard matching to follow HTTP proxy conventions (Nginx, Envoy,
Cloudflare, HAProxy) where `*.example.com` matches any subdomain depth. AWS
Network Firewall and Kubernetes Ingress use single-label strict matching, but
those are endpoint/routing systems. Since Airlock is an HTTP proxy, proxy
conventions are the appropriate reference.

The implementation change is minimal — the old check stripped the suffix and
then verified the remaining label contained no dots. The new check only
requires the prefix to be non-empty and end with a dot:

```rust
// before
Some(label) => !label.is_empty() && !label.contains('.'),

// after
Some(prefix) => prefix.len() > 1 && prefix.ends_with('.'),
```

Bare apex (`example.com` against `*.example.com`) still does not match, which
is consistent across all systems reviewed.

## Conflict checker alignment

The startup passthrough/middleware conflict checker had its own copy of
wildcard matching logic that was not updated alongside the matcher, leaving
two gaps:

**Wildcard × literal** — the checker's `wildcard_matches_literal` still
rejected multi-label hosts via `!label.contains('.')`. A passthrough rule
`*.example.com` would pass validation against middleware target
`a.b.example.com`, even though at runtime passthrough silently wins and
middleware never runs. Fixed by replacing the local helper with a direct call
to `matchers::host_matches`, so the two can never diverge again.

**Wildcard × wildcard** — the overlap check was `sa == sb` (equal suffixes
only). With multi-label matching, `*.example.com` and `*.prod.example.com`
overlap because `x.prod.example.com` matches both, but the checker accepted
this config. Fixed by also flagging when one suffix is a dot-separated
sub-suffix of the other: `sa == sb || sa ends with "."+sb || sb ends with "."+sa`.
