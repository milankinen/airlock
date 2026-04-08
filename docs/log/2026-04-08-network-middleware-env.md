# Network middleware env vars and rule refactoring

## Middleware env via subst templates

`NetworkMiddleware` gains an `env` map. Keys are the names exposed to the
Lua script as the `env` global table; values are subst templates expanded
from the host environment at compile time. A template that references an
undefined host variable resolves to nil in the script.

```toml
[[network.rules.my-api.middleware]]
env.TOKEN = "${MY_API_KEY}"
script = """
if env.TOKEN then
    req:setHeader("Authorization", "Bearer " .. env.TOKEN)
end
"""
```

## Flat middleware list per rule

`NetworkRule.middleware` changed from `BTreeMap<host, Vec<scripts>>` (host-
keyed) to `Vec<NetworkMiddleware>` (flat list). All middleware scripts on a
rule apply to every allowed target in that rule. This simplifies the config
schema and the rule resolution logic.

## TLS passthrough via ResolvedTarget

`tls_passthrough_from_config` was removed. Passthrough logic now lives
entirely in `ResolvedTarget.is_passthrough()`: a target gets passthrough if
it has no middleware and is not http-only. The list was previously also sent
to the supervisor, but the supervisor never read it — that RPC field is now
unused.

## "ezpez" CN on leaf certificates

Generated leaf certificates now carry CN `"ezpez <hostname>"` so intercepted
connections are clearly identifiable in browser certificate viewers. The CA
already had `"ezpez CA"`.
