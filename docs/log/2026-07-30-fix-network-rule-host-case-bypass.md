# Fix network rule bypass via hostname case / trailing dot

## Symptom

The network filter could be bypassed by the guest. With a `deny-by-default`
policy that allows a zone and denies one host inside it — the documented
"broad allow, specific deny" pattern:

```toml
[network.rules.example]
allow = ["*.example.com"]
deny  = ["secret.example.com"]
```

a guest connecting to `SECRET.example.com` (uppercased) or
`secret.example.com.` (trailing dot) was **allowed**: the deny rule failed to
match, the wildcard allow matched, and `TcpStream::connect` then resolved the
name case-insensitively and reached the blocked host anyway. The same trick
defeated every deny rule under `allow-by-default`.

## Root cause

`host_matches` in `network/matchers.rs` compared hostnames byte-for-byte
(`host == pattern`, and `strip_suffix` for `*.` wildcards). DNS and the OS
resolver are case-insensitive and treat a trailing dot (the DNS root label) as
insignificant, so the literal comparison was stricter than the resolver — a
mismatch that a deny rule (checked with an exact/wildcard match) fails open on
while the allow rule (broad wildcard) still fires.

`host_matches` is the single chokepoint for all rule evaluation — both
`NetworkTarget::matches` and `MiddlewareTarget::matches` route through it — so
the fix lands in one place and cannot be bypassed by any individual caller.

## Fix

Canonicalize both the incoming host and the rule pattern before comparing:
lowercase (ASCII) and strip a single trailing `.`. The `*` / `*.` wildcard
markers are ASCII and pass through unchanged; the apex-vs-wildcard rule
(`example.com` must not match `*.example.com`) is preserved because the
trailing-dot strip only removes the root label, not the leading structure.

Chosen over normalizing at rule-compile time plus at every call site because a
single canonicalization inside the matcher is bypass-proof: any future caller
of `host_matches` inherits the correct behavior for free. The per-call
allocation is negligible (matching runs once per TCP connect, not per byte).

## Tests

Added regression tests in `network/matchers.rs`:

- `matching_is_case_insensitive` — uppercased host vs lowercase rule (and the
  reverse), including the wildcard and localhost paths.
- `trailing_dot_is_ignored` — `host.` matches a rule written without the dot,
  while `example.com.` still does not match `*.example.com`.

Renamed the old `wildcard_is_case_sensitive_and_suffix_exact` to
`wildcard_suffix_is_exact` (it only ever asserted suffix exactness; the
"case sensitive" claim was the bug).
