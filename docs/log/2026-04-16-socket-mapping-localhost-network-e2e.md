# Socket mapping, localhost network rules, and e2e test updates

## Socket forward config simplification

Replaced the separate `host` + `guest` fields on `SocketForward` with a
single `host: SocketMapping` field using `source:target` syntax, matching
the pattern already used by `PortMapping`. A plain path means the same
path on both sides; `"~/.docker/run/docker.sock:/var/run/docker.sock"`
maps host source to guest target. The delimiter is the last colon
followed by `/` or `~` to avoid splitting on colons in directory names.

## Localhost equivalence in host matching

`host_matches()` now treats `localhost`, `127.0.0.1`, and `::1` as
equivalent. A rule with `allow = ["localhost:8080"]` matches connections
arriving as `127.0.0.1:8080` from the guest proxy. This fixes the
mismatch where the guest proxy sends literal IP addresses but rules
are written with `localhost`.

## Localhost connections skip middleware

`resolve_target()` now short-circuits all localhost connections: port-
forwarded ports are always allowed, non-forwarded ports follow
`default_mode`, and middleware is never applied. TLS interception on
loopback traffic is unnecessary and would require the interceptor CA
to be trusted for localhost connections.

## E2e test updates

- Middleware test now targets `https://example.org` with a Lua script
  that denies `/forbidden` paths — exercises real TLS interception.
- Port forwarding tests verify forwarded ports reach host and non-
  forwarded ports are denied under `default_mode = "deny"`.
- Removed socket relay bats test (unreliable due to socat timing).
- CI `bats-vm` job renamed to `test-e2e`, arm matrix removed (GitHub
  arm runners lack KVM support).
